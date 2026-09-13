//! Repository tests (T-002). Use-case ids are in the test names; docs/07 sections in comments.

use std::fmt::Debug;
use std::path::PathBuf;
use std::time::Instant;

use rusqlite::params;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::interpret::event_region_sql;
use super::inventory::EMITTER_REGION_SQL;
use super::measure::DETECTION_REGION_SQL;
use super::{RepoError, Repository, SCHEMA_VERSION, blob, region_bounds};
use crate::detection::SpurReason;
use crate::*;

// ---- fixtures ----

/// 2026-09 plus `sec` seconds.
fn t(sec: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + sec * 1_000_000_000)
}

fn tr(a: i64, b: i64) -> TimeRange {
    TimeRange::new(t(a), t(b))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("hk-model-repo-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sample_plan() -> ScanPlan {
    ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "ism-and-adsb".into(),
        created_at: t(0),
        regions: vec![
            PlanRegion {
                freq: FreqRange::new(902e6, 928e6),
                priority: 1.0,
                revisit_ns: Some(60_000_000_000),
            },
            PlanRegion {
                freq: FreqRange::new(1089e6, 1091e6),
                priority: 2.0,
                revisit_ns: None,
            },
        ],
        policy: ScanPolicy::SweepThenDwell,
        gain_table: vec![GainTableEntry {
            freq: FreqRange::new(1e6, 6e9),
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            antenna_port: Some("A1".into()),
        }],
        schedule: Schedule::Cron {
            expr: "0 */2 * * *".into(),
        },
        extra: json!({"dwell_budget_s": 30}),
    }
}

fn sample_survey(plan: &ScanPlan) -> Survey {
    Survey {
        id: SurveyId::new(),
        plan_id: plan.id,
        plan_version: plan.version,
        device_id: "synthetic:t-002".into(),
        state: SurveyState::Open,
        t_start: t(0),
        t_end: None,
        summary: None,
    }
}

fn sample_cal() -> CalibrationState {
    CalibrationState {
        id: CalibrationStateId::new(),
        supersedes: None,
        device_id: "synthetic:t-002".into(),
        ppm: -1.7,
        method: CalibrationMethod::FmPilot,
        measured_at: t(-60),
        valid: Some(tr(-60, 86_400)),
        temperature_c: Some(38.5),
        power_table: vec![PowerCalPoint {
            f_hz: 1090e6,
            gain_db: 44.0,
            offset_db: -71.5,
            gain: None,
            uncertainty_db: None,
        }],
    }
}

fn sample_spur() -> SpurMask {
    SpurMask {
        id: SpurMaskId::new(),
        supersedes: None,
        device_id: "synthetic:t-002".into(),
        measured_at: t(-120),
        rules: vec![
            SpurRule::Spur {
                freq: FreqRange::centered(1000e6, 1e3),
                level_dbfs: -62.0,
                gain_db: Some(44.0),
            },
            SpurRule::IqImage { rejection_db: 40.0 },
            SpurRule::CenterLeak { half_width_hz: 5e3 },
        ],
    }
}

fn sample_provenance(cal: CalibrationStateId, spur: SpurMaskId) -> Provenance {
    Provenance {
        device_id: "synthetic:t-002".into(),
        tune: Tune {
            center_hz: 1090e6,
            sample_rate_hz: 8e6,
            lna_db: 32.0,
            vga_db: 24.0,
            amp_on: true,
            bandwidth_hz: 7e6,
        },
        overload: false,
        quantisation_limited: false,
        temperature_c: Some(41.0),
        antenna_port: None,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: Some(cal),
        spur_mask_ref: Some(spur),
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: Some(1_000),
    }
}

fn det(
    survey: SurveyId,
    prov: ProvenanceId,
    f_center_hz: f64,
    obw_hz: f64,
    time: TimeRange,
) -> Detection {
    Detection {
        id: DetectionId::new(),
        survey_id: survey,
        time,
        f_center_hz,
        obw_hz,
        xdb_bandwidth_hz: Some(obw_hz * 1.2),
        xdb_level_db: Some(26.0),
        snr_peak_db: 18.5,
        snr_mean_db: 12.0,
        peak_level_dbfs: -42.5,
        peak_level_dbm: Some(-95.25),
        sk: Some(3.2),
        clip_count: 0,
        detector_version: "hk-detect/cfar@0.1.0;pfa=1e-6".into(),
        provenance_ref: prov,
        flags: DetectionFlags {
            marginal: true,
            ..DetectionFlags::default()
        },
    }
}

/// A repository with plan, survey, calibration, spur mask and one provenance row stored.
struct Base {
    repo: Repository,
    plan: ScanPlan,
    survey: Survey,
    cal: CalibrationState,
    spur: SpurMask,
    prov: Provenance,
    prov_id: ProvenanceId,
}

fn base_in(repo: Repository) -> Base {
    let mut repo = repo;
    let plan = sample_plan();
    let survey = sample_survey(&plan);
    let cal = sample_cal();
    let spur = sample_spur();
    repo.insert_scan_plan(&plan).unwrap();
    repo.insert_survey(&survey).unwrap();
    repo.insert_calibration_state(&cal).unwrap();
    repo.insert_spur_mask(&spur).unwrap();
    let prov = sample_provenance(cal.id, spur.id);
    let prov_id = repo.intern_provenance(&prov).unwrap();
    Base {
        repo,
        plan,
        survey,
        cal,
        spur,
        prov,
        prov_id,
    }
}

fn base() -> Base {
    base_in(Repository::open_in_memory().unwrap())
}

/// Every persisted docs/07 object, stored.
struct Graph {
    base: Base,
    detections: Vec<Detection>,
    track: Track,
    emitter: Emitter,
    recording: Recording,
    demod: Demodulation,
    decode: Decode,
    bitstream: Bitstream,
    annotation: Annotation,
    event: ExternalEvent,
    anomaly: Anomaly,
    explanation: Explanation,
}

fn graph() -> Graph {
    let mut b = base();
    let (survey_id, prov_id) = (b.survey.id, b.prov_id);
    let mut flagged = det(survey_id, prov_id, 915.0e6, 40e3, tr(40, 41));
    flagged.clip_count = 3;
    flagged.flags = DetectionFlags {
        clipped: true,
        spur_candidate: true,
        spur_reason: Some(SpurReason::SpurMap { mask: b.spur.id }),
        image_candidate: true,
        image_retune_confirmed: true,
        marginal: false,
        suspect_imd: true,
        compressed: true,
        impulsive: true,
        edge: true,
    };
    let detections = vec![det(survey_id, prov_id, 915.0e6, 40e3, tr(10, 11)), flagged];
    b.repo.insert_detections(&detections).unwrap();

    let track = Track {
        id: TrackId::new(),
        state: TrackState::Closed,
        split_from: None,
        time: tr(10, 41),
        f_center_hz: 915.0e6,
        bandwidth_hz: 40e3,
        detection_count: 2,
        timing: TimingFeatures {
            period_s: Some(30.0),
            duty_cycle: Some(0.033),
            inter_arrival_mean_s: Some(30.0),
            inter_arrival_std_s: Some(0.4),
            ..TimingFeatures::default()
        },
        updated_at: t(42),
    };
    b.repo.upsert_track(&track).unwrap();
    let ids: Vec<_> = detections.iter().map(|d| d.id).collect();
    b.repo
        .link_detections_to_track(track.id, &ids, t(42))
        .unwrap();

    let emitter = Emitter {
        id: EmitterId::new(),
        f_center_hz: 915.0e6,
        bandwidth_hz: 40e3,
        first_seen: t(10),
        last_seen: t(41),
        count: 2,
        fingerprint: json!({"symbol_rate_hz": 4800.0, "sync": "2dd4"}),
        identity: Identity::Unknown,
        known_status: KnownStatus::Unknown,
        classifications: vec![Classification {
            t: t(42),
            family: "2fsk".into(),
            confidence: 0.62,
            open_set_score: 0.81,
            model_version: "amc-baseline@0.1.0".into(),
        }],
        tags: ["ism".to_string()].into(),
    };
    b.repo.insert_emitter(&emitter).unwrap();

    let recording = Recording {
        id: RecordingId::new(),
        meta_uri: "recordings/2026/09/13/burst.sigmf-meta".into(),
        data_uri: "recordings/2026/09/13/burst.sigmf-data".into(),
        kind: RecordingKind::IqSnippet,
        time: TimeRange::new(t(9), t(12)),
        f_center_hz: 915.0e6,
        sample_rate_hz: 2e6,
        trigger: RecordingTrigger::Detection(detections[0].id),
        pre_trigger_s: 1.0,
        post_trigger_s: 1.0,
        size_bytes: 12_000_000,
        retention_class: RetentionClass::Unknown,
        content_class: ContentClass::Unrestricted,
        provenance_ref: prov_id,
    };
    b.repo.insert_recording(&recording).unwrap();

    let demod = Demodulation {
        id: DemodulationId::new(),
        emitter_ref: Some(emitter.id),
        detection_ref: Some(detections[0].id),
        recording_ref: Some(recording.id),
        mode: "2fsk".into(),
        params: EstimatedParams {
            symbol_rate_hz: Some(4800.0),
            deviation_hz: Some(20e3),
            cfo_hz: Some(-1250.0),
            mod_order: Some(2),
            roll_off: None,
            bandwidth_hz: Some(40e3),
        },
        lock_quality: Some(0.93),
        evm_db: Some(-17.5),
        time: tr(9, 12),
        demod_version: "hk-demod/fsk@0.1.0".into(),
    };
    b.repo.insert_demodulation(&demod).unwrap();

    let decode = Decode {
        id: DecodeId::new(),
        demodulation_ref: Some(demod.id),
        recording_ref: None,
        decoder_id: "hk-infer".into(),
        decoder_version: "0.1.0".into(),
        frame_model: "inferred:preamble-aaaa-sync-2dd4".into(),
        metadata: json!({"frame_len_bits": 32, "sync": "2dd4"}),
        content: Some(json!({"payload_hex": "2dd4a1b2"})),
        crc_status: CrcStatus::Unknown,
        identity: None,
        content_class: ContentClass::Unrestricted,
        t: t(10),
    };
    b.repo.insert_decode(&decode).unwrap();

    let bitstream = Bitstream {
        id: BitstreamId::new(),
        emitter_ref: Some(emitter.id),
        demodulation_ref: Some(demod.id),
        framing: Framing {
            payload: BitstreamPayload::HardBits,
            bits_per_symbol: Some(1),
            symbol_rate_hz: Some(4800.0),
            schema_id: None,
            sync_word_hex: Some("2dd4".into()),
        },
        transport: BitstreamTransport::Stored {
            uri: "bits/2026/09/13/burst.bits".into(),
        },
        time: tr(9, 12),
        provenance_ref: Some(prov_id),
        content_class: ContentClass::Unrestricted,
    };
    b.repo.insert_bitstream(&bitstream).unwrap();

    let annotation = Annotation {
        id: AnnotationId::new(),
        target: AnnotationTarget::Recording(RecordingSpan {
            recording_id: recording.id,
            sample_start: Some(2_000_000),
            sample_count: Some(96_000),
            region: Some(Region::new(FreqRange::centered(915.0e6, 40e3), tr(10, 11))),
        }),
        author: AnnotationAuthor::User,
        author_ref: "daniel".into(),
        kind: AnnotationKind::Label,
        value: "unknown/2fsk".into(),
        metadata: serde_json::Value::Null,
        content: None,
        confidence: 0.7,
        supersedes: None,
        content_class: ContentClass::Unrestricted,
        t: t(50),
        exported: false,
    };
    b.repo.insert_annotation(&annotation).unwrap();

    let event = ExternalEvent {
        id: ExternalEventId::new(),
        source: "gpsjam".into(),
        native_id: "2026-09-13/8a2a1072b59ffff".into(),
        event_type: "jamming-cell".into(),
        time: tr(0, 86_399),
        geo: Geo::BoundingBox {
            south_deg: 37.7,
            west_deg: -122.5,
            north_deg: 37.9,
            east_deg: -122.3,
        },
        freq: vec![FreqRange::new(1559e6, 1610e6)],
        payload: json!({"bad_fraction": 0.23}),
        fetched_at: t(100),
        valid_until: Some(t(172_800)),
    };
    let (_, event_hash) = b.repo.upsert_external_event(&event).unwrap();

    // docs/07 §5.2: GNSS L1 noise-floor rise explained by a cached gpsjam cell.
    let anomaly = Anomaly {
        id: AnomalyId::new(),
        kind: AnomalyKind::NoiseFloorRise,
        subject: AnomalySubject::Region,
        region: Region::new(FreqRange::new(1574.4e6, 1576.4e6), tr(3600, 7200)),
        score: 6.5,
        baseline_ref: Some("baseline:l1:24h".into()),
        t: t(3700),
        detector_version: "hk-noise@0.1.0".into(),
    };
    b.repo.insert_anomaly(&anomaly).unwrap();

    let explanation = Explanation {
        id: ExplanationId::new(),
        anomaly_ref: anomaly.id,
        cause: Cause::ExternalEvent { id: event.id },
        correlation_type: CorrelationType::TimeCoincidence,
        score: 0.82,
        evidence: vec![
            Evidence::ExternalEvent {
                id: event.id,
                payload_hash: event_hash,
            },
            Evidence::Value {
                name: "lag_s".into(),
                value: 0.0,
            },
            Evidence::History {
                region: anomaly.region,
            },
        ],
        supersedes: None,
        provisional: false,
        rule_version: "gnss-jam@0.1.0".into(),
        t: t(3800),
    };
    b.repo.insert_explanation(&explanation).unwrap();

    Graph {
        base: b,
        detections,
        track,
        emitter,
        recording,
        demod,
        decode,
        bitstream,
        annotation,
        event,
        anomaly,
        explanation,
    }
}

fn serde_round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value, "serde round trip of {json}");
}

fn index_names(repo: &Repository) -> Vec<String> {
    let mut stmt = repo
        .conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

// ---- schema ----

#[test]
fn schema_migrates_in_wal_mode_with_region_and_time_indexes() {
    let dir = TempDir::new();
    let path = dir.0.join("hackriff.db");
    {
        let repo = Repository::open(&path).unwrap();
        assert_eq!(repo.journal_mode().unwrap(), "wal");
        assert_eq!(repo.schema_version().unwrap(), SCHEMA_VERSION);
        let names = index_names(&repo);
        for required in [
            "idx_detection_f_center_t_start",
            "idx_detection_f_lo_f_hi",
            "idx_emitter_f_lo_f_hi",
            "idx_emitter_last_seen",
            "idx_emitter_status_emitter",
            "idx_anomaly_f_lo_f_hi",
            "idx_anomaly_t_start_t_end",
            "idx_explanation_f_lo_f_hi",
            "idx_explanation_t_start_t_end",
        ] {
            assert!(names.iter().any(|n| n == required), "missing {required}");
        }
    }
    // Reopening applies nothing and keeps data.
    let mut repo = Repository::open(&path).unwrap();
    assert_eq!(repo.schema_version().unwrap(), SCHEMA_VERSION);
    repo.checkpoint().unwrap();
    repo.conn
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(repo);
    assert!(matches!(
        Repository::open(&path),
        Err(RepoError::SchemaTooNew { .. })
    ));
}

// ---- round trips ----

#[test]
fn serde_round_trips_every_docs07_object() {
    let g = graph();
    let b = &g.base;
    serde_round_trip(&b.plan);
    serde_round_trip(&b.survey);
    serde_round_trip(&b.cal);
    serde_round_trip(&b.spur);
    serde_round_trip(&b.prov);
    for d in &g.detections {
        serde_round_trip(d);
    }
    serde_round_trip(&g.track);
    serde_round_trip(&g.emitter);
    serde_round_trip(&g.recording);
    serde_round_trip(&g.annotation);
    serde_round_trip(&g.demod);
    serde_round_trip(&g.decode);
    serde_round_trip(&g.bitstream);
    serde_round_trip(&g.event);
    serde_round_trip(&g.anomaly);
    serde_round_trip(&g.explanation);
    serde_round_trip(&EmitterObservation {
        emitter_id: g.emitter.id,
        seen: tr(0, 1),
        count: 3,
        f_center_hz: 1090e6,
        bandwidth_hz: 2e6,
        identity: Some(DecodedIdentity {
            scheme: IdentityScheme::AdsbIcao,
            value: "a1b2c3".into(),
        }),
    });
    serde_round_trip(&EmitterLink {
        emitter_id: g.emitter.id,
        target: LinkTarget::Track(g.track.id),
        linked_at: t(1),
    });
    serde_round_trip(&KnownStatusChange {
        emitter_id: g.emitter.id,
        status: KnownStatus::UnexpectedHere,
        prior_ref: Some("bandplan:us-fcc-2026#118-137MHz".into()),
        reason: "wfm in the aeronautical band".into(),
        t: t(5),
        author: StatusAuthor::Prior,
    });
    serde_round_trip(&AnomalyStatusChange {
        anomaly_id: g.anomaly.id,
        status: AnomalyStatus::Dismissed,
        t: t(2),
        note: Some("my own laptop".into()),
    });
    serde_round_trip(&TrackState::MergedInto(g.track.id));

    // Frames and tiles are not stored in SQLite, but are docs/07 objects too.
    let key = FrameKey {
        survey_id: b.survey.id,
        seq: 7,
    };
    serde_round_trip(&SweepFrame {
        key,
        t: t(3),
        freq: FreqRange::new(88e6, 108e6),
        bin_width_hz: 100e3,
        unit: PowerUnit::Dbfs,
        power: (0..200).map(|i| -90.5 + (i % 7) as f32).collect(),
        provenance_ref: b.prov_id,
    });
    serde_round_trip(&SpectrumFrame {
        key,
        t: t(3),
        f_center_hz: 915e6,
        span_hz: 8e6,
        rbw_hz: 1953.125,
        unit: PowerUnit::Dbm,
        psd: vec![-110.0, -80.5, -111.25],
        persistence: Some(Persistence {
            floor: -120.0,
            step_db: 1.0,
            levels: 2,
            counts: vec![1, 0, 0, 1, 1, 0],
        }),
        sk: Some(vec![1.0, 4.5, 0.98]),
        provenance_ref: b.prov_id,
    });
    serde_round_trip(&SpectrumTile {
        key: TileKey {
            scheme: 1,
            level: 2,
            f_block: 1090,
            t_block: 29_816,
        },
        freq: FreqRange::new(1090e6, 1091e6),
        time: tr(0, 60),
        bin_width_hz: 250e3,
        unit: PowerUnit::Dbfs,
        calibration_ref: Some(b.cal.id),
        suspect_fraction: 0.0,
        stats: TileStats {
            max: vec![-60.0; 4],
            mean: vec![-85.0; 4],
            low_percentile: vec![-95.0; 4],
            percentile: 10.0,
            observed: vec![true, true, false, true],
        },
    });
}

#[test]
fn db_round_trips_every_persisted_object() {
    let g = graph();
    let r = &g.base.repo;
    assert_eq!(r.scan_plan(g.base.plan.id, 1).unwrap(), g.base.plan);
    assert_eq!(r.survey(g.base.survey.id).unwrap(), g.base.survey);
    assert_eq!(r.calibration_state(g.base.cal.id).unwrap(), g.base.cal);
    assert_eq!(r.spur_mask(g.base.spur.id).unwrap(), g.base.spur);
    assert_eq!(r.provenance(g.base.prov_id).unwrap(), g.base.prov);
    for d in &g.detections {
        assert_eq!(&r.detection(d.id).unwrap(), d, "all flags and spur reason");
    }
    assert_eq!(r.track(g.track.id).unwrap(), g.track);
    assert_eq!(
        r.track_detections(g.track.id).unwrap(),
        g.detections.iter().map(|d| d.id).collect::<Vec<_>>()
    );
    assert_eq!(r.emitter(g.emitter.id).unwrap(), g.emitter);
    let history = r.known_status_history(g.emitter.id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        (history[0].status, history[0].author),
        (KnownStatus::Unknown, StatusAuthor::System)
    );
    assert_eq!(r.recording(g.recording.id).unwrap(), g.recording);
    assert_eq!(
        r.recordings_for_detection(g.detections[0].id).unwrap(),
        vec![g.recording.clone()]
    );
    assert_eq!(r.demodulation(g.demod.id).unwrap(), g.demod);
    assert_eq!(r.decode(g.decode.id).unwrap(), g.decode);
    assert_eq!(r.bitstream(g.bitstream.id).unwrap(), g.bitstream);
    assert_eq!(r.annotation(g.annotation.id).unwrap(), g.annotation);
    assert_eq!(
        r.annotations_for(&g.annotation.target).unwrap(),
        vec![g.annotation.clone()]
    );
    assert_eq!(r.external_event(g.event.id).unwrap(), g.event);
    assert_eq!(r.anomaly(g.anomaly.id).unwrap(), g.anomaly);
    assert_eq!(r.explanation(g.explanation.id).unwrap(), g.explanation);
    assert_eq!(
        r.explanations_for_anomaly(g.anomaly.id).unwrap(),
        vec![g.explanation.clone()]
    );
    assert!(matches!(
        r.detection(DetectionId::new()),
        Err(RepoError::NotFound {
            kind: "detection",
            ..
        })
    ));
}

#[test]
fn scan_plan_edits_are_new_versions_and_surveys_close_once() {
    let mut b = base();
    let mut v2 = b.plan.clone();
    v2.version = 2;
    v2.regions.pop();
    b.repo.insert_scan_plan(&v2).unwrap();
    assert_eq!(b.repo.latest_scan_plan(b.plan.id).unwrap(), v2);
    assert_eq!(
        b.repo.scan_plan(b.plan.id, 1).unwrap(),
        b.plan,
        "v1 unchanged"
    );
    let mut skip = v2.clone();
    skip.version = 4;
    assert!(matches!(
        b.repo.insert_scan_plan(&skip),
        Err(RepoError::Invalid(_))
    ));

    let summary = SurveySummary {
        detections: 12,
        dropped_samples: 3,
        ..SurveySummary::default()
    };
    b.repo
        .finish_survey(b.survey.id, SurveyState::Closed, t(600), &summary)
        .unwrap();
    let closed = b.repo.survey(b.survey.id).unwrap();
    assert_eq!(closed.state, SurveyState::Closed);
    assert_eq!(closed.t_end, Some(t(600)));
    assert_eq!(closed.summary, Some(summary.clone()));
    assert!(matches!(
        b.repo
            .finish_survey(b.survey.id, SurveyState::Aborted, t(700), &summary),
        Err(RepoError::Invalid(_))
    ));
}

// ---- provenance ----

#[test]
fn identical_provenance_values_share_one_row_by_content_hash() {
    let mut b = base();
    assert_eq!(
        b.repo.intern_provenance(&b.prov.clone()).unwrap(),
        b.prov_id
    );

    let mut hot = b.prov.clone();
    hot.overload = true;
    let other = b.repo.intern_provenance(&hot).unwrap();
    assert_ne!(other, b.prov_id);
    assert_eq!(b.repo.intern_provenance(&hot).unwrap(), other);

    // -0.0 and 0.0 are the same value, so they share a row.
    let mut pos = b.prov.clone();
    pos.temperature_c = Some(0.0);
    let mut neg = b.prov.clone();
    neg.temperature_c = Some(-0.0);
    assert_eq!(
        b.repo.intern_provenance(&pos).unwrap(),
        b.repo.intern_provenance(&neg).unwrap()
    );

    let (rows, hash_len): (i64, i64) = b
        .repo
        .conn
        .query_row(
            "SELECT count(*), min(length(content_hash)) FROM provenance",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((rows, hash_len), (3, 32));

    let mut nan = b.prov.clone();
    nan.temperature_c = Some(f64::NAN);
    assert!(matches!(
        b.repo.intern_provenance(&nan),
        Err(RepoError::Invalid(_))
    ));
}

/// docs/07 §2.6: every Detection has a resolvable provenance chain.
#[test]
fn provenance_chain_resolves_from_detection_to_calibration_and_spur_mask() {
    let g = graph();
    let r = &g.base.repo;
    let d = r.detection(g.detections[1].id).unwrap();
    let chain = r.provenance_chain(d.provenance_ref).unwrap();
    assert_eq!(chain.id, g.base.prov_id);
    assert_eq!(chain.provenance, g.base.prov);
    assert_eq!(chain.calibration, Some(g.base.cal.clone()));
    assert_eq!(chain.spur_mask, Some(g.base.spur.clone()));
    assert_eq!(
        d.flags.spur_reason.and_then(|s| s.mask()),
        Some(g.base.spur.id)
    );

    // The schema refuses a dangling reference, so the chain cannot break.
    let mut b = base();
    let mut orphan = b.prov.clone();
    orphan.calibration_state_ref = Some(CalibrationStateId::new());
    assert!(matches!(
        b.repo.intern_provenance(&orphan),
        Err(RepoError::Engine(_))
    ));
    let dangling = det(b.survey.id, ProvenanceId::new(), 1e6, 1e3, tr(0, 1));
    assert!(b.repo.insert_detection(&dangling).is_err());
}

// ---- measurement vs interpretation ----

#[test]
fn measurements_are_immutable_and_interpretations_append_only() {
    let g = graph();
    let conn = &g.base.repo.conn;
    let rejected = [
        (
            "UPDATE detection SET snr_peak = 99 WHERE detection_id = ?1",
            blob(g.detections[0].id),
        ),
        (
            "UPDATE provenance SET overload = 1 WHERE provenance_id = ?1",
            blob(g.base.prov_id),
        ),
        (
            "UPDATE recording SET size_bytes = 0 WHERE recording_id = ?1",
            blob(g.recording.id),
        ),
        (
            "UPDATE calibration_state SET device_id = 'x' WHERE cal_id = ?1",
            blob(g.base.cal.id),
        ),
        (
            "UPDATE decode SET crc_status = 'valid' WHERE decode_id = ?1",
            blob(g.decode.id),
        ),
        (
            "UPDATE explanation SET score = 1 WHERE explanation_id = ?1",
            blob(g.explanation.id),
        ),
        (
            "UPDATE emitter_classification SET family = 'x' WHERE emitter_id = ?1",
            blob(g.emitter.id),
        ),
        (
            "UPDATE emitter_status SET status = 'known' WHERE emitter_id = ?1",
            blob(g.emitter.id),
        ),
        (
            "UPDATE annotation SET body = '{}' WHERE annotation_id = ?1",
            blob(g.annotation.id),
        ),
    ];
    for (sql, id) in rejected {
        let err = conn.execute(sql, [id]).unwrap_err().to_string();
        assert!(
            err.contains("immutable") || err.contains("append-only"),
            "{sql}: {err}"
        );
    }
    // The export flag is the one permitted annotation change.
    let mut repo = g.base.repo;
    repo.mark_annotations_exported(&[g.annotation.id]).unwrap();
    assert!(repo.annotation(g.annotation.id).unwrap().exported);
}

// ---- content gating (ADR-0004, legal guardrail) ----

fn expect_gated(result: Result<(), RepoError>, object: &str, class: ContentClass) {
    match result {
        Err(RepoError::GatedContent {
            object: o,
            class: c,
        }) => {
            assert_eq!((o, c), (object, class));
        }
        other => panic!("{object} under {class:?}: expected GatedContent, got {other:?}"),
    }
}

/// Tries every content-carrying insert path under a gated `class`: content is refused,
/// metadata-only forms are accepted, and the schema refuses content from raw SQL too.
fn assert_every_insert_path_gates(class: ContentClass) {
    assert!(!class.permits_content());
    let mut g = graph();
    let class_text = serde_json::to_value(class).unwrap();
    let class_text = class_text.as_str().unwrap();
    let repo = &mut g.base.repo;

    // Decode: content refused; metadata still recorded.
    let mut decode = g.decode.clone();
    decode.id = DecodeId::new();
    decode.content_class = class;
    decode.content = Some(json!({"message": "call me at 555-0100"}));
    expect_gated(repo.insert_decode(&decode), "decode", class);
    decode.content = None;
    repo.insert_decode(&decode).unwrap();
    let stored = repo.decode_ungated(decode.id).unwrap();
    assert_eq!(
        (stored.content, stored.metadata),
        (None, g.decode.metadata.clone())
    );
    // T-036: the gated reads withhold a restricted row's metadata, even with authorisation.
    for access in [
        IdentityAccess::Standard,
        IdentityAccess::OwnTrafficAuthorised,
    ] {
        let view = repo.decode_with_access(decode.id, access).unwrap();
        assert!(view.metadata_withheld);
        assert_eq!(view.decode.metadata, json!({}));
    }

    // Annotation: content refused; the label itself is metadata.
    let mut ann = g.annotation.clone();
    ann.id = AnnotationId::new();
    ann.content_class = class;
    ann.content = Some(json!({"message": "call me at 555-0100"}));
    expect_gated(repo.insert_annotation(&ann), "annotation", class);
    ann.content = None;
    ann.metadata = json!({"address": 1234567, "frames": 3});
    repo.insert_annotation(&ann).unwrap();

    // Recording: IQ is content, so a gated class is refused outright.
    let mut rec = g.recording.clone();
    rec.id = RecordingId::new();
    rec.content_class = class;
    expect_gated(repo.insert_recording(&rec), "recording", class);

    // Bitstream: stored bits refused; a live descriptor is metadata.
    let mut bits = g.bitstream.clone();
    bits.id = BitstreamId::new();
    bits.content_class = class;
    bits.transport = BitstreamTransport::Stored {
        uri: "bits/gated.bits".into(),
    };
    expect_gated(repo.insert_bitstream(&bits), "bitstream", class);
    bits.transport = BitstreamTransport::Live {
        endpoint: "unix:///run/hackriff/meta.sock".into(),
    };
    repo.insert_bitstream(&bits).unwrap();

    // The schema repeats the rule for writers that bypass the repository.
    for sql in [
        "INSERT INTO decode (decode_id, decoder_id, crc_status, content_class, has_content, t, body) \
         VALUES (?1, 'raw', 'valid', ?2, 1, 0, '{}')",
        "INSERT INTO annotation (annotation_id, target_kind, author, kind, content_class, \
         has_content, t, exported, body) VALUES (?1, 'region', 'user', 'label', ?2, 1, 0, 0, '{}')",
        "INSERT INTO bitstream (bitstream_id, transport, content_class, t_start, body) \
         VALUES (?1, 'stored', ?2, 0, '{}')",
    ] {
        let err = repo
            .conn
            .execute(sql, params![blob(DecodeId::new()), class_text])
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHECK"), "{sql}: {err}");
    }
    let err = repo
        .conn
        .execute(
            "INSERT INTO recording (recording_id, kind, t_start, t_end, f_center, trigger_kind, \
             size_bytes, retention_class, content_class, provenance_id, body) \
             VALUES (?1, 'iq-snippet', 0, 1, 1e6, 'manual', 1, 'routine', ?2, ?3, '{}')",
            params![blob(RecordingId::new()), class_text, blob(g.base.prov_id)],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("CHECK"), "raw recording: {err}");
}

#[test]
fn gating_metadata_only_refuses_content_on_every_insert_path() {
    assert_every_insert_path_gates(ContentClass::MetadataOnly);
}

#[test]
fn gating_restricted_cellular_refuses_content_on_every_insert_path() {
    assert_every_insert_path_gates(ContentClass::RestrictedCellular);
}

#[test]
fn gating_restricted_paging_refuses_content_on_every_insert_path() {
    assert_every_insert_path_gates(ContentClass::RestrictedPaging);
}

/// A new gated ContentClass variant must get its own gating test above.
#[test]
fn gating_tests_cover_every_gated_variant() {
    let gated: Vec<_> = ContentClass::ALL
        .iter()
        .copied()
        .filter(|c| !c.permits_content())
        .collect();
    assert_eq!(
        gated,
        [
            ContentClass::MetadataOnly,
            ContentClass::RestrictedCellular,
            ContentClass::RestrictedPaging
        ]
    );
    assert!(gated.contains(&ContentClass::FAIL_CLOSED));
}

#[test]
fn permitted_classes_carry_content_on_every_insert_path() {
    let mut g = graph();
    for class in [ContentClass::Unrestricted, ContentClass::OwnKeyDecrypted] {
        let repo = &mut g.base.repo;
        let mut decode = g.decode.clone();
        decode.id = DecodeId::new();
        decode.content_class = class;
        repo.insert_decode(&decode).unwrap();
        let mut ann = g.annotation.clone();
        ann.id = AnnotationId::new();
        ann.content_class = class;
        ann.content = Some(json!({"text": "own traffic"}));
        repo.insert_annotation(&ann).unwrap();
        let mut rec = g.recording.clone();
        rec.id = RecordingId::new();
        rec.content_class = class;
        repo.insert_recording(&rec).unwrap();
        let mut bits = g.bitstream.clone();
        bits.id = BitstreamId::new();
        bits.content_class = class;
        repo.insert_bitstream(&bits).unwrap();
    }
}

// ---- detections ----

#[test]
fn detection_batch_insert_is_one_transaction() {
    let mut b = base();
    let batch: Vec<_> = (0..1_000)
        .map(|i| {
            det(
                b.survey.id,
                b.prov_id,
                433.92e6 + i as f64 * 10.0,
                10e3,
                tr(i, i + 1),
            )
        })
        .collect();
    b.repo.insert_detections(&batch).unwrap();
    assert_eq!(b.repo.detection_count().unwrap(), 1_000);

    // A bad row mid-batch (unknown provenance) rolls back the whole batch.
    let mut bad: Vec<_> = (0..10)
        .map(|i| det(b.survey.id, b.prov_id, 868e6, 10e3, tr(i, i + 1)))
        .collect();
    bad[5].provenance_ref = ProvenanceId::new();
    assert!(b.repo.insert_detections(&bad).is_err());
    assert_eq!(b.repo.detection_count().unwrap(), 1_000);
}

/// Overload is sticky tune-state: it (and any clipped samples) must propagate to
/// `flags.clipped` on every detection.
#[test]
fn overload_and_clipping_propagate_to_detection_flags() {
    let mut b = base();
    let mut hot = b.prov.clone();
    hot.overload = true;
    let hot_id = b.repo.intern_provenance(&hot).unwrap();

    let mut under_overload = det(b.survey.id, hot_id, 433.92e6, 10e3, tr(0, 1));
    let id = under_overload.id;
    assert!(matches!(
        b.repo.insert_detection(&under_overload),
        Err(RepoError::UnflaggedClipping { detection }) if detection == id
    ));
    under_overload.flags.clipped = true;
    under_overload.flags.suspect_imd = true;
    b.repo.insert_detection(&under_overload).unwrap();

    let mut clipped = det(b.survey.id, b.prov_id, 433.92e6, 10e3, tr(2, 3));
    clipped.clip_count = 5;
    assert!(matches!(
        b.repo.insert_detection(&clipped),
        Err(RepoError::UnflaggedClipping { .. })
    ));
    clipped.flags.clipped = true;
    b.repo.insert_detection(&clipped).unwrap();
    assert_eq!(b.repo.detection(clipped.id).unwrap().clip_count, 5);

    // Dependent flags must be consistent.
    let mut orphan_reason = det(b.survey.id, b.prov_id, 433.92e6, 10e3, tr(4, 5));
    orphan_reason.flags.spur_reason = Some(SpurReason::Dc);
    assert!(matches!(
        b.repo.insert_detection(&orphan_reason),
        Err(RepoError::Invalid(_))
    ));

    // The trigger and CHECKs enforce the same rules for raw SQL.
    let raw = "INSERT INTO detection (detection_id, survey_id, provenance_id, t_start, t_end, \
               f_center, obw, f_lo, f_hi, snr_peak, snr_mean, flags, peak_dbfs, clip_count, \
               detector_version) VALUES (?1, ?2, ?3, 0, 1, 1e6, 1e3, 999500, 1000500, 10, 5, \
               0, -40, ?4, 'raw')";
    let err = b
        .repo
        .conn
        .execute(
            raw,
            params![blob(DetectionId::new()), blob(b.survey.id), blob(hot_id), 0],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("flags.clipped"), "{err}");
    let err = b
        .repo
        .conn
        .execute(
            raw,
            params![
                blob(DetectionId::new()),
                blob(b.survey.id),
                blob(b.prov_id),
                9
            ],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("CHECK"), "{err}");
}

/// AWARE-042 (region over time; docs/07 §4 step 2): the region query returns exactly the
/// detections whose frequency × time box overlaps the query, including very wide and very long
/// ones and ones touching the query edge.
#[test]
fn aware_042_region_over_time_query_returns_only_overlapping_detections() {
    let mut b = base();
    let (s, p) = (b.survey.id, b.prov_id);
    let mut all = Vec::new();
    for fi in 0..41 {
        for ti in 0..20 {
            all.push(det(
                s,
                p,
                100e6 + fi as f64 * 250e3,
                25e3,
                half_second_from(ti),
            ));
        }
    }
    let wide = det(s, p, 80e6, 50e6, tr(5, 6)); // 55–105 MHz
    let long = det(s, p, 105e6, 25e3, tr(-990, 10)); // ends inside the query window
    let touching = det(s, p, 106.0125e6, 25e3, tr(11, 12)); // f_lo == 106 MHz, t_start == 11 s
    let far = det(s, p, 2400e6, 20e6, tr(9, 10));
    all.extend([wide.clone(), long.clone(), touching.clone(), far.clone()]);
    for chunk in all.chunks(128) {
        b.repo.insert_detections(chunk).unwrap();
    }

    let query = Region::new(FreqRange::new(104e6, 106e6), tr(9, 11));
    let got = b.repo.detections_in_region(&query).unwrap();
    let mut got_ids: Vec<_> = got.iter().map(|d| d.id).collect();
    let mut want: Vec<_> = all
        .iter()
        .filter(|d| d.region().overlaps(&query))
        .map(|d| d.id)
        .collect();
    got_ids.sort();
    want.sort();
    assert_eq!(got_ids, want);
    assert!(got.iter().all(|d| d.region().overlaps(&query)));
    for must in [&long, &touching] {
        assert!(got_ids.contains(&must.id));
    }
    assert!(!got_ids.contains(&wide.id), "wide detection ended at 6 s");
    assert!(!got_ids.contains(&far.id));
    assert!(got.windows(2).all(|w| w[0].time.start <= w[1].time.start));

    // The wide one is found when the window includes its time.
    let early = Region::new(FreqRange::new(104e6, 104.1e6), tr(5, 5));
    assert!(
        b.repo
            .detections_in_region(&early)
            .unwrap()
            .iter()
            .any(|d| d.id == wide.id)
    );
}

/// A row wider and longer than anything before it, committed by another connection after a
/// query, is found by the next query (extent bounds are read in the query's snapshot).
#[test]
fn a_wide_row_inserted_after_a_query_is_found_by_the_next_query() {
    let dir = TempDir::new();
    let path = dir.0.join("wide.db");
    let mut writer = base_in(Repository::open(&path).unwrap());
    let reader = Repository::open(&path).unwrap();
    let (s, p) = (writer.survey.id, writer.prov_id);
    let query = Region::new(FreqRange::new(400e6, 401e6), tr(50, 60));

    writer
        .repo
        .insert_detection(&det(s, p, 400.5e6, 10e3, tr(55, 56)))
        .unwrap();
    assert_eq!(reader.detections_in_region(&query).unwrap().len(), 1);

    let wide = det(s, p, 300e6, 250e6, tr(-10_000, 51)); // 175–425 MHz, ends inside the window
    writer.repo.insert_detection(&wide).unwrap();
    let got = reader.detections_in_region(&query).unwrap();
    assert_eq!(got.len(), 2);
    assert!(got.iter().any(|d| d.id == wide.id));
}

/// Two WAL connections doing read-then-write transactions concurrently: with `BEGIN IMMEDIATE`
/// they queue on the busy timeout instead of failing with `SQLITE_BUSY_SNAPSHOT`, and provenance
/// interning converges on one row per value.
#[test]
fn concurrent_wal_connections_read_then_write_without_busy_snapshot() {
    let dir = TempDir::new();
    let path = dir.0.join("concurrent.db");
    let b = base_in(Repository::open(&path).unwrap());
    let emitter_id = EmitterId::new();
    const N: u64 = 100;
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let prov = b.prov.clone();
            std::thread::spawn(move || -> Result<ProvenanceId, RepoError> {
                let mut repo = Repository::open(&path)?;
                let q = Region::new(FreqRange::new(433e6, 435e6), tr(0, 1_000_000));
                let mut last = None;
                for i in 0..N {
                    // A read on this connection, then read-then-write transactions that race the
                    // other connection's writes.
                    repo.emitters_in_region(&q)?;
                    repo.upsert_emitter_observation(&EmitterObservation {
                        emitter_id,
                        seen: tr(i as i64, i as i64 + 1),
                        count: 1,
                        f_center_hz: 433.92e6,
                        bandwidth_hz: 20e3,
                        identity: None,
                    })?;
                    let mut p = prov.clone();
                    p.antenna_port = Some(format!("port-{}", i % 3));
                    last = Some(repo.intern_provenance(&p)?);
                }
                Ok(last.expect("N > 0"))
            })
        })
        .collect();
    let ids: Vec<ProvenanceId> = workers
        .into_iter()
        .map(|h| h.join().unwrap().unwrap())
        .collect();
    assert_eq!(
        ids[0], ids[1],
        "same value, same row, from both connections"
    );
    let e = b.repo.emitter(emitter_id).unwrap();
    assert_eq!(e.count, 2 * N);
    let rows: i64 = b
        .repo
        .conn
        .query_row("SELECT count(*) FROM provenance", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1 + 3, "base provenance plus one row per antenna port");
}

#[test]
fn track_detections_are_ordered_by_detection_time_not_id() {
    let mut b = base();
    let (a, c) = (DetectionId::new(), DetectionId::new());
    let (low_id, high_id) = if a < c { (a, c) } else { (c, a) };
    let mut late = det(b.survey.id, b.prov_id, 433.92e6, 10e3, tr(100, 101));
    late.id = low_id;
    let mut early = det(b.survey.id, b.prov_id, 433.92e6, 10e3, tr(10, 11));
    early.id = high_id;
    b.repo
        .insert_detections(&[late.clone(), early.clone()])
        .unwrap();
    let track = Track {
        id: TrackId::new(),
        state: TrackState::Open,
        split_from: None,
        time: tr(10, 101),
        f_center_hz: 433.92e6,
        bandwidth_hz: 10e3,
        detection_count: 2,
        timing: TimingFeatures::default(),
        updated_at: t(102),
    };
    b.repo.upsert_track(&track).unwrap();
    b.repo
        .link_detections_to_track(track.id, &[late.id, early.id], t(102))
        .unwrap();
    assert_eq!(
        b.repo.track_detections(track.id).unwrap(),
        vec![early.id, late.id]
    );
}

/// A 0.5 s span starting at `sec`.
fn half_second_from(sec: i64) -> TimeRange {
    TimeRange::new(t(sec), t(sec).saturating_add_nanos(500_000_000))
}

fn query_plan<P: rusqlite::Params>(repo: &Repository, sql: &str, params: P) -> Vec<String> {
    let mut stmt = repo
        .conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap();
    stmt.query_map(params, |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn assert_uses_index(plan: &[String], table: &str) {
    assert!(
        plan.iter()
            .any(|l| l.starts_with(&format!("SEARCH {table} "))
                && l.contains(&format!("INDEX idx_{table}_"))),
        "{table} query does not search a region/time index: {plan:?}"
    );
    assert!(
        !plan.iter().any(|l| l.starts_with(&format!("SCAN {table}"))),
        "{table} query scans the table: {plan:?}"
    );
}

/// ADR-0006 risk: region/time queries must stay index range searches, not table scans.
#[test]
fn region_and_time_queries_use_indexes() {
    let g = graph();
    let r = &g.base.repo;
    let q = Region::new(FreqRange::new(900e6, 930e6), tr(0, 3600));

    let d = region_bounds(&r.conn, "detection", &q)
        .unwrap()
        .detection_params();
    let plan = query_plan(
        r,
        DETECTION_REGION_SQL,
        params![d.0, d.1, d.2, d.3, d.4, d.5, d.6],
    );
    eprintln!("detection region plan: {plan:?}");
    assert_uses_index(&plan, "detection");

    let e = region_bounds(&r.conn, "emitter", &q).unwrap();
    let plan = query_plan(
        r,
        EMITTER_REGION_SQL,
        params![e.f_lo_min, e.hi, e.lo, e.t0, e.t1],
    );
    eprintln!("emitter region plan: {plan:?}");
    assert_uses_index(&plan, "emitter");

    for table in ["anomaly", "explanation"] {
        let b = region_bounds(&r.conn, table, &q).unwrap();
        let plan = query_plan(
            r,
            &event_region_sql(table),
            params![b.f_lo_min, b.hi, b.lo, b.t_start_min, b.t1, b.t0],
        );
        eprintln!("{table} region plan: {plan:?}");
        assert_uses_index(&plan, table);
    }
}

/// ADR-0006 risk: SQLite write throughput under dense detection rates. Prints the batched
/// insert rate; the bound is deliberately loose so CI is not flaky.
#[test]
fn detection_write_throughput_is_reported() {
    let dir = TempDir::new();
    let mut b = base_in(Repository::open(dir.0.join("throughput.db")).unwrap());
    const BATCH: usize = 1_000;
    const BATCHES: usize = 20;
    let batches: Vec<Vec<Detection>> = (0..BATCHES)
        .map(|k| {
            (0..BATCH)
                .map(|i| {
                    let n = (k * BATCH + i) as i64;
                    det(
                        b.survey.id,
                        b.prov_id,
                        400e6 + (n % 5_000) as f64 * 1e3,
                        12.5e3,
                        TimeRange::new(t(n / 100), t(n / 100 + 1)),
                    )
                })
                .collect()
        })
        .collect();
    let start = Instant::now();
    for batch in &batches {
        b.repo.insert_detections(batch).unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    let rate = (BATCH * BATCHES) as f64 / secs;
    eprintln!(
        "T-002 detection insert throughput: {rate:.0} rows/s ({} rows in batches of {BATCH}, \
         file DB, WAL, synchronous=NORMAL, {} build)",
        BATCH * BATCHES,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    assert_eq!(b.repo.detection_count().unwrap(), (BATCH * BATCHES) as u64);
    assert!(
        rate > 1_000.0,
        "batched inserts unexpectedly slow: {rate:.0} rows/s"
    );
}

// ---- inventory (C27) ----

fn adsb(hex: &str) -> DecodedIdentity {
    DecodedIdentity {
        scheme: IdentityScheme::AdsbIcao,
        value: hex.into(),
    }
}

/// SIGNAL-001 (ADS-B) and docs/07 §2.11: two sessions of the same aircraft become one Emitter
/// with the count summed and first/last seen spanning both, even though the tracker minted a
/// fresh emitter id in each session.
#[test]
fn signal_001_adsb_icao_sessions_merge_into_one_emitter() {
    let mut b = base();
    let icao = adsb("a1b2c3");
    let day1 = b
        .repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: tr(1_000, 1_600),
            count: 120,
            f_center_hz: 1090e6,
            bandwidth_hz: 2e6,
            identity: Some(icao.clone()),
        })
        .unwrap();
    assert!(day1.created);
    let day2 = b
        .repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: tr(90_000, 90_300),
            count: 45,
            f_center_hz: 1090.001e6,
            bandwidth_hz: 2e6,
            identity: Some(icao.clone()),
        })
        .unwrap();
    assert_eq!(day2.emitter_id, day1.emitter_id);
    assert!(!day2.created);

    // The legacy writer records no identity class: gated reads withhold the ICAO (T-034), so the
    // aggregate is checked on the crate-private ungated read.
    assert_eq!(
        b.repo.emitter(day1.emitter_id).unwrap().identity,
        Identity::Unknown
    );
    assert!(b.repo.emitter_by_identity(&icao).unwrap().is_none());
    let e = b.repo.emitter_ungated(day1.emitter_id).unwrap();
    assert_eq!(e.count, 165);
    assert_eq!(e.seen(), tr(1_000, 90_300));
    assert_eq!(e.identity, Identity::Decoded(icao.clone()));
    assert_eq!(
        e.f_center_hz, 1090.001e6,
        "current frequency follows the latest session"
    );

    // An older session replayed late widens first_seen but keeps the current frequency.
    b.repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: day1.emitter_id,
            seen: tr(10, 20),
            count: 5,
            f_center_hz: 1089.9e6,
            bandwidth_hz: 2e6,
            identity: None,
        })
        .unwrap();
    let e = b.repo.emitter_ungated(day1.emitter_id).unwrap();
    assert_eq!((e.count, e.seen()), (170, tr(10, 90_300)));
    assert_eq!(e.f_center_hz, 1090.001e6);

    let everywhere = Region::new(FreqRange::new(1089e6, 1091e6), tr(0, 100_000));
    assert_eq!(b.repo.emitters_in_region(&everywhere).unwrap().len(), 1);
}

#[test]
fn identity_conflicts_are_left_to_entity_resolution() {
    let mut b = base();
    let first = b
        .repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: tr(0, 1),
            count: 1,
            f_center_hz: 1090e6,
            bandwidth_hz: 2e6,
            identity: Some(adsb("aaaaaa")),
        })
        .unwrap();
    let second = b
        .repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: tr(0, 1),
            count: 1,
            f_center_hz: 1090e6,
            bandwidth_hz: 2e6,
            identity: None,
        })
        .unwrap();
    // The existing, unidentified second emitter now decodes to the first one's identity.
    let err = b
        .repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: second.emitter_id,
            seen: tr(2, 3),
            count: 1,
            f_center_hz: 1090e6,
            bandwidth_hz: 2e6,
            identity: Some(adsb("aaaaaa")),
        })
        .unwrap_err();
    assert!(
        matches!(err, RepoError::IdentityConflict { existing, .. } if existing == first.emitter_id)
    );
    // An identified emitter cannot silently change identity either.
    let err = b
        .repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: first.emitter_id,
            seen: tr(2, 3),
            count: 1,
            f_center_hz: 1090e6,
            bandwidth_hz: 2e6,
            identity: Some(adsb("bbbbbb")),
        })
        .unwrap_err();
    assert!(matches!(err, RepoError::IdentityConflict { .. }));
    assert_eq!(
        b.repo.emitter(second.emitter_id).unwrap().count,
        1,
        "rolled back"
    );
}

/// AWARE-036 (mystery ISM burst; docs/07 §5.3): an unknown signal is an Emitter with no decoded
/// identity, `known_status: unknown` and a non-zero open-set score, and re-classification
/// appends history instead of overwriting it.
#[test]
fn aware_036_unknown_emitter_has_open_set_score_and_append_only_history() {
    let g = graph();
    let mut repo = g.base.repo;
    let id = g.emitter.id;
    // A second session of the same (unidentified) cluster, resolved by id.
    let up = repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: id,
            seen: tr(600, 900),
            count: 10,
            f_center_hz: 915.002e6,
            bandwidth_hz: 40e3,
            identity: None,
        })
        .unwrap();
    assert_eq!(
        up,
        EmitterUpsert {
            emitter_id: id,
            created: false
        }
    );
    repo.append_classification(
        id,
        &Classification {
            t: t(1_000),
            family: "unknown".into(),
            confidence: 0.4,
            open_set_score: 0.93,
            model_version: "amc-cnn@0.2.0".into(),
        },
    )
    .unwrap();
    for target in [
        LinkTarget::Track(g.track.id),
        LinkTarget::Recording(g.recording.id),
        LinkTarget::Demodulation(g.demod.id),
    ] {
        repo.link_emitter(&EmitterLink {
            emitter_id: id,
            target,
            linked_at: t(1_001),
        })
        .unwrap();
    }

    let e = repo.emitter(id).unwrap();
    assert_eq!(e.identity, Identity::Unknown);
    assert_eq!(e.known_status, KnownStatus::Unknown);
    assert_eq!((e.count, e.seen()), (12, tr(10, 900)));
    let current = e.current_classification().unwrap();
    assert!(current.open_set_score > 0.0);
    assert_eq!(current.model_version, "amc-cnn@0.2.0");
    assert_eq!(e.classifications.len(), 2);
    assert_eq!(
        e.classifications[0], g.emitter.classifications[0],
        "history kept"
    );
    let links = repo.emitter_links(id).unwrap();
    assert_eq!(links.len(), 3);
    assert!(
        links
            .iter()
            .any(|l| l.target == LinkTarget::Track(g.track.id))
    );
}

/// SIGNAL-062 (RDS): the RDS PI code is the emitter's identity; the PS name arrives as a
/// CRC-valid decode and a decoder ground-truth annotation on the emitter.
#[test]
fn signal_062_rds_pi_identity_and_ps_label() {
    let mut b = base();
    let pi = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "C0DE".into(),
    };
    let station = b
        .repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: tr(0, 120),
            count: 1,
            f_center_hz: 88.5e6,
            bandwidth_hz: 200e3,
            identity: Some(pi.clone()),
        })
        .unwrap()
        .emitter_id;
    b.repo
        .append_known_status(&KnownStatusChange {
            emitter_id: station,
            status: KnownStatus::Known,
            prior_ref: Some("fmlist:pi/C0DE".into()),
            reason: "RDS PI matches a licensed station on this frequency".into(),
            t: t(121),
            author: StatusAuthor::Prior,
        })
        .unwrap();
    let decode = Decode {
        id: DecodeId::new(),
        demodulation_ref: None,
        recording_ref: None,
        decoder_id: "hk-rds".into(),
        decoder_version: "0.1.0".into(),
        frame_model: "rds-group-0a".into(),
        metadata: json!({"pi": "C0DE", "pty": 3, "group": "0A"}),
        content: Some(json!({"ps": "KQED    "})),
        crc_status: CrcStatus::Valid,
        identity: Some(pi.clone()),
        content_class: ContentClass::Unrestricted,
        t: t(30),
    };
    b.repo.insert_decode(&decode).unwrap();
    let label = Annotation {
        id: AnnotationId::new(),
        target: AnnotationTarget::Emitter(station),
        author: AnnotationAuthor::Decoder,
        author_ref: "hk-rds@0.1.0".into(),
        kind: AnnotationKind::GroundTruth,
        value: "fm/rds".into(),
        metadata: json!({"pi": "C0DE", "crc_valid_groups": 12}),
        content: Some(json!({"ps": "KQED"})),
        confidence: 1.0,
        supersedes: None,
        content_class: ContentClass::Unrestricted,
        t: t(31),
        exported: false,
    };
    b.repo.insert_annotation(&label).unwrap();
    b.repo
        .link_emitter(&EmitterLink {
            emitter_id: station,
            target: LinkTarget::Decode(decode.id),
            linked_at: t(31),
        })
        .unwrap();

    let e = b.repo.emitter_by_identity(&pi).unwrap().unwrap();
    assert_eq!(e.id, station);
    assert_eq!(e.known_status, KnownStatus::Known);
    let decodes = b.repo.decodes_for_identity(&pi).unwrap();
    assert_eq!(decodes, vec![decode.clone()]);
    assert_eq!(decodes[0].crc_status, CrcStatus::Valid);
    assert_eq!(decodes[0].content.as_ref().unwrap()["ps"], "KQED    ");
    let labels = b
        .repo
        .annotations_for(&AnnotationTarget::Emitter(station))
        .unwrap();
    assert_eq!(labels, vec![label]);
    assert_eq!(labels[0].kind, AnnotationKind::GroundTruth);
    assert_eq!(labels[0].content.as_ref().unwrap()["ps"], "KQED");
    assert_eq!(
        b.repo.emitter_links(station).unwrap()[0].target,
        LinkTarget::Decode(decode.id)
    );
}

/// AWARE-053: an emitter that matches priors but is not expected here gets
/// `known_status: unexpected-here` as an appended status entry, the inventory region query
/// surfaces it, and later changes keep the earlier entries.
#[test]
fn aware_053_unexpected_here_status_is_an_append_only_history() {
    let mut b = base();
    let id = b
        .repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: tr(0, 300),
            count: 40,
            f_center_hz: 121.95e6,
            bandwidth_hz: 150e3,
            identity: None,
        })
        .unwrap()
        .emitter_id;
    b.repo
        .append_classification(
            id,
            &Classification {
                t: t(301),
                family: "wfm".into(),
                confidence: 0.91,
                open_set_score: 0.04,
                model_version: "amc-baseline@0.1.0".into(),
            },
        )
        .unwrap();
    let unexpected = KnownStatusChange {
        emitter_id: id,
        status: KnownStatus::UnexpectedHere,
        prior_ref: Some("bandplan:us-fcc-2026#118-137MHz:aeronautical-mobile".into()),
        reason: "broadcast-style WFM carrier inside the aeronautical band".into(),
        t: t(302),
        author: StatusAuthor::Prior,
    };
    b.repo.append_known_status(&unexpected).unwrap();
    b.repo.add_emitter_tag(id, "out-of-allocation").unwrap();
    b.repo.add_emitter_tag(id, "out-of-allocation").unwrap();

    let airband = Region::new(FreqRange::new(118e6, 137e6), tr(200, 400));
    let found = b.repo.emitters_in_region(&airband).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].known_status, KnownStatus::UnexpectedHere);
    assert!(found[0].tags.contains("out-of-allocation"));
    assert!(
        b.repo
            .emitters_in_region(&Region::new(FreqRange::new(118e6, 137e6), tr(301, 400)))
            .unwrap()
            .is_empty(),
        "not seen after 300 s"
    );

    // A user later identifies it (e.g. an authorised event transmitter): appended, not overwritten.
    b.repo
        .append_known_status(&KnownStatusChange {
            emitter_id: id,
            status: KnownStatus::Known,
            prior_ref: None,
            reason: "authorised temporary airshow commentary transmitter".into(),
            t: t(400),
            author: StatusAuthor::User,
        })
        .unwrap();
    assert_eq!(b.repo.emitter(id).unwrap().known_status, KnownStatus::Known);
    let history = b.repo.known_status_history(id).unwrap();
    assert_eq!(
        history
            .iter()
            .map(|h| (h.status, h.author))
            .collect::<Vec<_>>(),
        [
            (KnownStatus::Unknown, StatusAuthor::System),
            (KnownStatus::UnexpectedHere, StatusAuthor::Prior),
            (KnownStatus::Known, StatusAuthor::User),
        ]
    );
    assert_eq!(history[1], unexpected);

    assert!(b.repo.remove_emitter_tag(id, "out-of-allocation").unwrap());
    assert!(matches!(
        b.repo.append_known_status(&KnownStatusChange {
            emitter_id: EmitterId::new(),
            ..unexpected
        }),
        Err(RepoError::NotFound { .. })
    ));
}

// ---- attack map (docs/07 §5.2) ----

#[test]
fn attack_map_anomalies_and_explanations_are_region_indexed_and_evidence_is_pinned() {
    let g = graph();
    let mut repo = g.base.repo;

    let l1 = Region::new(FreqRange::new(1575.0e6, 1575.9e6), tr(5_000, 5_001));
    assert_eq!(
        repo.anomalies_in_region(&l1).unwrap(),
        vec![g.anomaly.clone()]
    );
    assert_eq!(
        repo.explanations_in_region(&l1).unwrap(),
        vec![g.explanation.clone()]
    );
    let later = Region::new(l1.freq, tr(7_201, 9_000));
    assert!(repo.anomalies_in_region(&later).unwrap().is_empty());
    assert!(repo.explanations_in_region(&later).unwrap().is_empty());

    repo.append_anomaly_status(&AnomalyStatusChange {
        anomaly_id: g.anomaly.id,
        status: AnomalyStatus::Resolved,
        t: t(8_000),
        note: Some("gpsjam cell confirmed".into()),
    })
    .unwrap();
    let history = repo.anomaly_status_history(g.anomaly.id).unwrap();
    assert_eq!(
        history.iter().map(|h| h.status).collect::<Vec<_>>(),
        [AnomalyStatus::Open, AnomalyStatus::Resolved]
    );

    // A refreshed feed fact keeps its id but changes its payload hash, so the stored evidence is
    // visibly stale rather than silently pointing at different data.
    let Evidence::ExternalEvent {
        payload_hash: pinned,
        ..
    } = g.explanation.evidence[0]
    else {
        panic!("first evidence is the external event");
    };
    let mut refreshed = g.event.clone();
    refreshed.id = ExternalEventId::new();
    refreshed.payload = json!({"bad_fraction": 0.31});
    refreshed.fetched_at = t(7_000);
    let (id, current) = repo.upsert_external_event(&refreshed).unwrap();
    assert_eq!(id, g.event.id);
    assert_ne!(current, pinned);
    let cached = repo.external_event(g.event.id).unwrap();
    assert_eq!(cached.payload_hash().unwrap(), current);
    assert_eq!(repo.explanation(g.explanation.id).unwrap(), g.explanation);

    // New evidence must pin the current payload.
    let stale = Explanation {
        id: ExplanationId::new(),
        ..g.explanation.clone()
    };
    assert!(matches!(
        repo.insert_explanation(&stale),
        Err(RepoError::StaleEvidence { event, .. }) if event == g.event.id
    ));
    let mut fresh = stale.clone();
    fresh.evidence[0] = Evidence::ExternalEvent {
        id: g.event.id,
        payload_hash: current,
    };
    fresh.supersedes = Some(g.explanation.id);
    repo.insert_explanation(&fresh).unwrap();
    assert_eq!(
        repo.explanations_for_anomaly(g.anomaly.id).unwrap().len(),
        2
    );
}

/// The `spur_reason` CHECK accepts `clock-harmonic` (T-006 review; pre-release in-place edit of
/// 0001) and keeps refusing unknown reasons; a clock-harmonic detection round-trips.
#[test]
fn clock_harmonic_spur_reason_round_trips() {
    let mut b = base_in(Repository::open_in_memory().unwrap());
    assert_eq!(b.repo.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 1);
    let mut d = det(b.survey.id, b.prov_id, 434.0e6, 1.5e3, tr(10, 11));
    d.flags.spur_candidate = true;
    d.flags.spur_reason = Some(SpurReason::ClockHarmonic);
    b.repo.insert_detection(&d).unwrap();
    assert_eq!(b.repo.detection(d.id).unwrap(), d);
    let raw = b.repo.conn.execute(
        "INSERT INTO detection (detection_id, survey_id, provenance_id, t_start, t_end, f_center, \
         obw, f_lo, f_hi, snr_peak, snr_mean, flags, peak_dbfs, clip_count, detector_version, \
         spur_reason) VALUES (?1, ?2, ?3, 0, 1, 1e6, 1e3, 999500, 1000500, 10, 5, 2, -40, 0, 'x', \
         'not-a-reason')",
        params![blob(DetectionId::new()), blob(b.survey.id), blob(b.prov_id)],
    );
    assert!(raw.is_err(), "unknown spur reasons stay refused");
    // A fresh file database accepts it too.
    let dir = TempDir::new();
    let mut file = base_in(Repository::open(dir.0.join("fresh.sqlite")).unwrap());
    assert_eq!(file.repo.schema_version().unwrap(), 1);
    let mut e = det(file.survey.id, file.prov_id, 434.0e6, 1.5e3, tr(12, 13));
    e.flags.spur_candidate = true;
    e.flags.spur_reason = Some(SpurReason::ClockHarmonic);
    file.repo.insert_detection(&e).unwrap();
    assert_eq!(file.repo.detection(e.id).unwrap(), e);
}
