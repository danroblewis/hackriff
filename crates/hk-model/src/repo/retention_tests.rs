//! T-904 detection retention tests. The assertions are on the docs/07 objects a client reads —
//! the inventory entry, its presence, its latest measurement, the track — before and after a
//! prune, never on the prune's internals.

use super::Repository;
use crate::cluster::*;
use crate::*;

/// 2026-09 plus `ms` milliseconds.
fn t(ms: i64) -> Timestamp {
    Timestamp::from_unix_nanos(1_789_000_000_000_000_000 + ms * 1_000_000)
}

fn tr(a_ms: i64, b_ms: i64) -> TimeRange {
    TimeRange::new(t(a_ms), t(b_ms))
}

const S: i64 = 1_000;
const NS: i64 = 1_000_000;

struct World {
    repo: Repository,
    survey: SurveyId,
    prov: ProvenanceId,
}

fn world_in(mut repo: Repository) -> World {
    let plan = ScanPlan {
        id: ScanPlanId::new(),
        version: 1,
        name: "ism".into(),
        created_at: t(0),
        regions: vec![PlanRegion {
            freq: FreqRange::new(433e6, 435e6),
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
                center_hz: 433.92e6,
                sample_rate_hz: 2.4e6,
                lna_db: 24.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 1.75e6,
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
    World {
        repo,
        survey: survey.id,
        prov,
    }
}

fn world() -> World {
    world_in(Repository::open_in_memory().unwrap())
}

impl World {
    /// One per-frame detection; `snr` distinguishes rows so "which one is the latest" is visible.
    fn det(&self, f_hz: f64, time: TimeRange, snr: f64) -> Detection {
        Detection {
            id: DetectionId::new(),
            survey_id: self.survey,
            time,
            f_center_hz: f_hz,
            obw_hz: 12e3,
            xdb_bandwidth_hz: None,
            xdb_level_db: None,
            snr_peak_db: snr,
            snr_mean_db: snr - 3.0,
            peak_level_dbfs: -40.0,
            peak_level_dbm: None,
            sk: None,
            clip_count: 0,
            detector_version: "test@1".into(),
            provenance_ref: self.prov,
            flags: DetectionFlags::default(),
        }
    }

    /// A track of `frames` back-to-back 100 ms detections from `start_ms`, stored and linked.
    fn track(&mut self, f_hz: f64, start_ms: i64, frames: i64) -> (TrackId, Vec<Detection>) {
        let dets: Vec<Detection> = (0..frames)
            .map(|i| {
                let a = start_ms + i * 100;
                self.det(f_hz, tr(a, a + 100), 10.0 + i as f64 * 0.01)
            })
            .collect();
        self.repo.insert_detections(&dets).unwrap();
        let end = start_ms + frames * 100;
        let track = Track {
            id: TrackId::new(),
            state: TrackState::Open,
            split_from: None,
            time: tr(start_ms, end),
            f_center_hz: f_hz,
            bandwidth_hz: 12e3,
            detection_count: frames as u64,
            timing: TimingFeatures::default(),
            updated_at: t(end),
        };
        self.repo.upsert_track(&track).unwrap();
        let ids: Vec<DetectionId> = dets.iter().map(|d| d.id).collect();
        self.repo
            .link_detections_to_track(track.id, &ids, t(end))
            .unwrap();
        (track.id, dets)
    }

    /// The inventory entry a track sighting creates (the pipeline's live-offer path).
    fn sight(&mut self, track: TrackId, f_hz: f64, seen: TimeRange, count: u64) -> EmitterId {
        self.repo
            .record_sighting(
                &Sighting {
                    source: LinkTarget::Track(track),
                    seen,
                    count,
                    f_center_hz: f_hz,
                    bandwidth_hz: 12e3,
                    fingerprint: Some(Fingerprint::new(f_hz, 12e3)),
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

    fn remaining(&self, ids: &[Detection]) -> Vec<DetectionId> {
        ids.iter()
            .filter(|d| self.repo.detection(d.id).is_ok())
            .map(|d| d.id)
            .collect()
    }
}

fn policy(max_age_s: i64, keep: usize, batch: usize) -> DetectionRetention {
    DetectionRetention {
        max_age_ns: max_age_s * S * NS,
        keep_per_emitter: keep,
        batch,
        ..DetectionRetention::default()
    }
}

/// Everything a client reads for one inventory entry over one window.
#[derive(Debug, PartialEq)]
struct Seen {
    listed: bool,
    latest: Option<LatestMeasurement>,
    presence: Presence,
    track: Track,
}

fn seen(w: &World, id: EmitterId, track: TrackId, window: TimeRange, now: Timestamp) -> Seen {
    let page = w
        .repo
        .query_inventory(&InventoryQuery {
            time: Some(window),
            limit: 100,
            ..InventoryQuery::default()
        })
        .unwrap();
    Seen {
        listed: page.entries.iter().any(|e| e.emitter.id == id),
        latest: w.repo.emitter_latest_measurement(id).unwrap(),
        presence: w
            .repo
            .presence(id, window, IdleGap::conservative(), now)
            .unwrap(),
        track: w.repo.track(track).unwrap(),
    }
}

/// The acceptance test: a pruned window still answers the inventory, presence, the latest
/// measurement and the track for its signal, exactly as before; per-frame rows past the age are
/// gone except each emitter's newest tail; what went is summarised in rollups that account for
/// every deleted row. AWARE-042 (region over time) is the query the rollups keep answerable.
#[test]
fn a_pruned_window_still_answers_the_inventory_and_presence_aware_042() {
    let mut w = world();
    // An old continuous carrier: 60 s of 100 ms frames (600 rows), seen as one inventory entry.
    let (old_track, old) = w.track(433.92e6, 0, 600);
    let old_id = w.sight(old_track, 433.92e6, tr(0, 60 * S), 600);
    // A burst an hour later on another frequency: moves the watermark.
    let (burst_track, burst) = w.track(434.5e6, 3_600 * S, 5);
    let burst_id = w.sight(burst_track, 434.5e6, tr(3_600 * S, 3_600 * S + 500), 5);
    let old_window = tr(0, 60 * S);
    let now = t(3_600 * S + 500);
    let before = seen(&w, old_id, old_track, old_window, now);
    let burst_before = seen(&w, burst_id, burst_track, tr(3_600 * S, 3_601 * S), now);
    assert!(before.listed && before.latest.is_some());

    let report = w
        .repo
        .prune_detections(&policy(600, 16, 37), || true)
        .unwrap();

    // Nothing a client reads changed.
    assert_eq!(seen(&w, old_id, old_track, old_window, now), before);
    assert_eq!(
        seen(&w, burst_id, burst_track, tr(3_600 * S, 3_601 * S), now),
        burst_before
    );
    // Only the emitter's newest 16 per-frame rows of the old carrier remain; the burst is young.
    assert_eq!(
        w.remaining(&old),
        old[584..].iter().map(|d| d.id).collect::<Vec<_>>()
    );
    assert_eq!(w.remaining(&burst).len(), 5);
    assert_eq!(report.deleted, 584);
    assert_eq!(report.kept_tail, 16);
    assert!(report.complete);
    assert!(
        report.batches > 1,
        "several short write transactions, not one"
    );
    assert!(report.lock_ns_max > 0 && report.lock_ns_max <= report.lock_ns_total);
    // Every deleted row is in exactly one rollup of its track, contiguous and ≤ 60 s each.
    let rollups = w.repo.track_rollups(old_track).unwrap();
    assert_eq!(rollups.iter().map(|r| r.detections).sum::<u64>(), 584);
    assert!(rollups.iter().all(|r| r.track_id == Some(old_track)));
    assert!(rollups.iter().all(|r| r.time.duration_ns() <= 60 * S * NS));
    assert_eq!(rollups.first().unwrap().time.start, t(0));
    assert_eq!(rollups.last().unwrap().time.end, old[583].time.end);
    assert!(
        rollups
            .iter()
            .all(|r| r.freq.lo_hz <= 433.92e6 - 6e3 && r.freq.hi_hz >= 433.92e6 + 6e3)
    );
    // The region query over the pruned window finds the rollups (and the kept tail as rows).
    let region = Region::new(FreqRange::new(433.9e6, 433.95e6), tr(10 * S, 20 * S));
    assert!(w.repo.detections_in_region(&region).unwrap().is_empty());
    let r = w.repo.detection_rollups_in_region(&region).unwrap();
    assert!(!r.is_empty() && r.iter().all(|r| r.time.end >= t(10 * S)));
    // A second pass finds nothing more to do.
    let again = w
        .repo
        .prune_detections(&policy(600, 16, 37), || true)
        .unwrap();
    assert_eq!((again.deleted, again.kept_tail), (0, 16));
}

/// Class 2: a row something names by id — decode provenance, a recording trigger, a direct
/// emitter link — is never pruned, however old; and deleting its neighbours never trips a
/// foreign key.
#[test]
fn referenced_detections_are_pinned_forever() {
    let mut w = world();
    let (_, dets) = w.track(433.92e6, 0, 20);
    let (_, _young) = w.track(434.5e6, 7_200 * S, 1);
    let demod = Demodulation {
        id: DemodulationId::new(),
        emitter_ref: None,
        detection_ref: Some(dets[3].id),
        recording_ref: None,
        mode: "ook".into(),
        params: EstimatedParams::default(),
        lock_quality: None,
        evm_db: None,
        time: dets[3].time,
        demod_version: "test@1".into(),
    };
    w.repo.insert_demodulation(&demod).unwrap();
    let e = w
        .repo
        .record_sighting(
            &Sighting {
                source: LinkTarget::Detection(dets[7].id),
                seen: dets[7].time,
                count: 1,
                f_center_hz: 433.92e6,
                bandwidth_hz: 12e3,
                fingerprint: None,
                identity: None,
                context: None,
                classification: None,
                tags: Vec::new(),
            },
            None,
        )
        .unwrap()
        .emitter_id;
    let before = w.repo.emitter_latest_measurement(e).unwrap();
    let report = w
        .repo
        .prune_detections(&policy(60, 16, 5), || true)
        .unwrap();
    assert_eq!(w.remaining(&dets), vec![dets[3].id, dets[7].id]);
    assert_eq!(report.kept_pinned, 2);
    assert_eq!(report.deleted, 18);
    assert_eq!(w.repo.emitter_latest_measurement(e).unwrap(), before);
}

/// Rollups are contiguous runs: a gap longer than the policy's closes one, the span caps one,
/// and a later batch extends the track's newest rollup rather than starting a new row.
#[test]
fn rollups_split_on_gaps_and_span_and_extend_across_batches() {
    let mut w = world();
    // 0–1.0 s (10 frames), a 15 s silence (over the 10 s gap), 16.0–16.5 s (5 frames): two runs
    // of one track.
    let first: Vec<Detection> = (0..10)
        .map(|i| w.det(433.92e6, tr(i * 100, i * 100 + 100), 10.0))
        .collect();
    let second: Vec<Detection> = (0..5)
        .map(|i| w.det(433.92e6, tr(16_000 + i * 100, 16_100 + i * 100), 10.0))
        .collect();
    let all: Vec<Detection> = first.iter().chain(&second).cloned().collect();
    w.repo.insert_detections(&all).unwrap();
    let track = Track {
        id: TrackId::new(),
        state: TrackState::Open,
        split_from: None,
        time: tr(0, 16_500),
        f_center_hz: 433.92e6,
        bandwidth_hz: 12e3,
        detection_count: 15,
        timing: TimingFeatures::default(),
        updated_at: t(16_500),
    };
    w.repo.upsert_track(&track).unwrap();
    let ids: Vec<DetectionId> = all.iter().map(|d| d.id).collect();
    w.repo
        .link_detections_to_track(track.id, &ids, t(16_500))
        .unwrap();
    let _young = w.track(434.5e6, 3_600 * S, 1);
    let report = w
        .repo
        .prune_detections(&policy(60, 16, 3), || true)
        .unwrap();
    assert_eq!(report.deleted, 15);
    assert!(
        report.rollups_extended > 0,
        "batches of 3 continue one rollup"
    );
    let rollups = w.repo.track_rollups(track.id).unwrap();
    assert_eq!(
        rollups
            .iter()
            .map(|r| (r.time, r.on_air_ns, r.detections))
            .collect::<Vec<_>>(),
        vec![
            (tr(0, 1_000), 1_000 * NS, 10),
            (tr(16_000, 16_500), 500 * NS, 5)
        ]
    );
    // A 3 s flicker inside a run (under the gap) stays one rollup, and its hull is not its time
    // on air: 2 s of frames over a 5 s hull.
    let mut w = world();
    let flicker: Vec<Detection> = (0..10)
        .chain(30..40)
        .map(|i| w.det(433.92e6, tr(i * 100, i * 100 + 100), 10.0))
        .collect();
    w.repo.insert_detections(&flicker).unwrap();
    let ft = Track {
        id: TrackId::new(),
        time: tr(0, 4_000),
        ..track
    };
    w.repo.upsert_track(&ft).unwrap();
    let ids: Vec<DetectionId> = flicker.iter().map(|d| d.id).collect();
    w.repo
        .link_detections_to_track(ft.id, &ids, t(4_000))
        .unwrap();
    let _young = w.track(434.5e6, 3_600 * S, 1);
    w.repo
        .prune_detections(&policy(60, 0, 100), || true)
        .unwrap();
    let r = w.repo.track_rollups(ft.id).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(
        (r[0].time, r[0].on_air_ns, r[0].detections),
        (tr(0, 4_000), 2_000 * NS, 20)
    );

    // The span cap: 150 s of frames becomes three ≤ 60 s rollups.
    let mut w = world();
    let (long, _) = w.track(433.92e6, 0, 1_500);
    let _young = w.track(434.5e6, 7_200 * S, 1);
    w.repo
        .prune_detections(&policy(60, 0, 400), || true)
        .unwrap();
    let r = w.repo.track_rollups(long).unwrap();
    assert_eq!(r.len(), 3);
    assert!(r.iter().all(|r| r.time.duration_ns() <= 60 * S * NS));
    assert_eq!(r.iter().map(|r| r.detections).sum::<u64>(), 1_500);
}

/// With rollup off the rows simply age out; the age is measured from the newest stored row, never
/// the wall clock (these rows are from a fixed 2026 instant and a year-old replay is not old).
#[test]
fn age_is_measured_from_the_newest_row_and_rollup_off_just_deletes() {
    let mut w = world();
    let (track, dets) = w.track(433.92e6, 0, 50);
    let all_young = w
        .repo
        .prune_detections(&policy(86_400, 0, 10), || true)
        .unwrap();
    assert_eq!((all_young.examined, all_young.deleted), (0, 0));
    assert_eq!(w.remaining(&dets).len(), 50);

    let _young = w.track(434.5e6, 7_200 * S, 1);
    let off = DetectionRetention {
        rollup: false,
        ..policy(60, 0, 10)
    };
    let report = w.repo.prune_detections(&off, || true).unwrap();
    assert_eq!(report.deleted, 50);
    assert!(w.repo.track_rollups(track).unwrap().is_empty());
    // The track itself is durable.
    assert_eq!(w.repo.track(track).unwrap().detection_count, 50);
}

/// `between` stops a pass; the next pass resumes and finishes the job.
#[test]
fn a_stopped_pass_resumes() {
    let mut w = world();
    let (_, dets) = w.track(433.92e6, 0, 40);
    let _young = w.track(434.5e6, 7_200 * S, 1);
    let mut calls = 0;
    let first = w
        .repo
        .prune_detections(&policy(60, 0, 10), || {
            calls += 1;
            calls < 2
        })
        .unwrap();
    assert!(!first.complete);
    assert_eq!(first.deleted, 20);
    let rest = w
        .repo
        .prune_detections(&policy(60, 0, 10), || true)
        .unwrap();
    assert!(rest.complete);
    assert_eq!(rest.deleted, 20);
    assert!(w.remaining(&dets).is_empty());
}

/// The race guard: a track linked to an emitter **during** a pass (another connection, between
/// two batches) gets that emitter's newest tail protected for the rest of the pass — the cached
/// "unlinked" answer is dropped, not trusted.
#[test]
fn a_link_made_during_a_pass_protects_the_new_emitters_tail() {
    let dir = std::env::temp_dir().join(format!("hk-t904-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hackriff.db");
    let mut w = world_in(Repository::open(&path).unwrap());
    let (track, dets) = w.track(433.92e6, 0, 30);
    let _young = w.track(434.5e6, 7_200 * S, 1);
    let mut other = Repository::open(&path).unwrap();
    let mut linked = None;
    let report = w
        .repo
        .prune_detections(&policy(60, 8, 5), || {
            if linked.is_none() {
                let e = other
                    .record_sighting(
                        &Sighting {
                            source: LinkTarget::Track(track),
                            seen: tr(0, 3_000),
                            count: 30,
                            f_center_hz: 433.92e6,
                            bandwidth_hz: 12e3,
                            fingerprint: Some(Fingerprint::new(433.92e6, 12e3)),
                            identity: None,
                            context: None,
                            classification: None,
                            tags: Vec::new(),
                        },
                        None,
                    )
                    .unwrap()
                    .emitter_id;
                linked = Some(e);
            }
            true
        })
        .unwrap();
    assert_eq!(report.relinks, 1);
    // The first batch (rows 0–4) went while the track was unlinked. The link landed before the
    // second batch took the lock, so that batch (rows 5–9) was kept and the track's cut
    // re-derived; from then on the new emitter's newest 8 are protected.
    assert_eq!(report.kept_moved, 5);
    assert_eq!(report.deleted, 5 + 12);
    let kept: Vec<DetectionId> = dets[5..10]
        .iter()
        .chain(&dets[22..])
        .map(|d| d.id)
        .collect();
    assert_eq!(w.remaining(&dets), kept);
    // The next pass prunes the rows it held back, and never the protected tail.
    let next = w.repo.prune_detections(&policy(60, 8, 5), || true).unwrap();
    assert_eq!((next.deleted, next.kept_tail, next.relinks), (5, 8, 0));
    assert_eq!(
        w.remaining(&dets),
        dets[22..].iter().map(|d| d.id).collect::<Vec<_>>()
    );
    let storage = w.repo.detection_storage().unwrap();
    assert_eq!(storage.detection_rows, 8 + 1);
    assert!(storage.rollup_rows >= 1);
    assert!(storage.db_bytes > 0 && storage.wal_bytes.is_some());
    assert_eq!(storage.oldest_detection, Some(dets[22].time));
    assert_eq!(storage.newest_detection_end, Some(t(7_200 * S + 100)));
    drop(other);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// One STFT frame yields several detections with the same `t_end`; a batch boundary that falls
/// among them must not skip any (the pass's cursor is `(t_end, detection_id)`, not `t_end`).
#[test]
fn rows_sharing_an_end_time_across_a_batch_boundary_are_all_pruned() {
    let mut w = world();
    let mut all = Vec::new();
    for f in [433.90e6, 433.95e6, 434.00e6] {
        let (_, dets) = w.track(f, 0, 7);
        all.extend(dets);
    }
    let _young = w.track(434.5e6, 7_200 * S, 1);
    let report = w.repo.prune_detections(&policy(60, 0, 2), || true).unwrap();
    assert_eq!(report.deleted, 21);
    assert!(w.remaining(&all).is_empty());
}
