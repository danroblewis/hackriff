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

/// Simulated steady ingest at the measured 2.4 Msps mock-replay rate (~20 rows/s: twelve 10 s
/// tracks of 100 back-to-back 100 ms detections per simulated minute), with `prune` applied every
/// second simulated minute. Returns the database bytes (`page_count × page_size`) after each
/// simulated minute.
fn steady_ingest(minutes: i64, prune: Option<&DetectionRetention>) -> (World, Vec<u64>) {
    let mut w = world();
    let mut sizes = Vec::new();
    for m in 0..minutes {
        for k in 0..12 {
            let start = m * 60 * S + (k % 6) * 10 * S;
            w.track(433.1e6 + k as f64 * 100e3, start, 100);
        }
        if let Some(p) = prune.filter(|_| m % 2 == 1) {
            w.repo.prune_detections(p, || true).unwrap();
        }
        sizes.push(w.repo.detection_storage().unwrap().db_bytes);
    }
    (w, sizes)
}

/// T-904 regression bound on growth, deterministic (simulated capture time, no wall clock): with
/// retention on the database stops growing once the age is reached — SQLite reuses the freed pages —
/// while the same ingest without it grows linearly. Tracks stay durable either way. The bytes/h
/// measured through the mock SDR replay are in the ticket's result; this pins the shape.
#[test]
fn a_steady_ingest_plateaus_under_retention_and_grows_without_it() {
    let policy = policy(300, 0, 100);
    let (kept_all, grow) = steady_ingest(30, None);
    let (pruned, flat) = steady_ingest(30, Some(&policy));
    // Without retention: minutes 10 → 30 at least doubles the file.
    assert!(grow[29] > 2 * grow[9], "unpruned growth {grow:?}");
    // With retention: after the age plus a couple of passes, the file grows < 15 % over 20 min.
    assert!(
        flat[29] * 100 < flat[9] * 115,
        "pruned store should plateau: {flat:?}"
    );
    assert!(flat[29] * 2 < grow[29], "{flat:?} vs {grow:?}");
    let s = pruned.repo.detection_storage().unwrap();
    // At most the age (5 min) plus one pass interval (2 min) of rows at 20 rows/s.
    assert!(s.detection_rows <= 7 * 60 * 20, "{s:?}");
    assert!(s.rollup_rows > 0);
    // Every track is still there.
    let tracks = |w: &World| -> i64 {
        w.repo
            .conn
            .query_row("SELECT count(*) FROM track", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(tracks(&pruned), 30 * 12);
    assert_eq!(tracks(&kept_all), 30 * 12);
}

/// T-904 / T-453, **timing tier** (`.config/nextest.toml`, `just timing`): one batch of a prune
/// pass holds SQLite's write lock briefly, so the detector's writer is never stalled behind it.
/// The assertion is a latency bound — a property of the machine's headroom as much as of the code
/// — so it runs on a quiet box, never in the gate (docs/10 §3.6). Measured through the mock SDR
/// replay under load ~44 on the dev Mac (2026-09-24): worst batch 26.6 ms, typical 4–12 ms.
#[test]
fn a_prune_batch_holds_the_write_lock_briefly() {
    let dir = std::env::temp_dir().join(format!("hk-t904-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hackriff.db");
    let mut w = world_in(Repository::open(&path).unwrap());
    // An hour of the 2.4 Msps replay's rate (~20 rows/s) on a file database, 40 min past the age.
    for m in 0..60 {
        for k in 0..12 {
            let start = m * 60 * S + (k % 6) * 10 * S;
            w.track(433.1e6 + k as f64 * 100e3, start, 100);
        }
    }
    let report = w
        .repo
        .prune_detections(&policy(20 * 60, 256, 100), || true)
        .unwrap();
    assert!(report.deleted > 40 * 60 * 15, "{report:?}");
    assert!(report.batches > 100, "{report:?}");
    let max_ms = report.lock_ns_max as f64 / 1e6;
    eprintln!(
        "prune: {} rows in {} batches, worst lock hold {max_ms:.2} ms, total {:.1} ms",
        report.deleted,
        report.batches,
        report.lock_ns_total as f64 / 1e6
    );
    assert!(
        max_ms < 50.0,
        "worst batch held the write lock {max_ms:.1} ms: {report:?}"
    );
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

impl World {
    /// Another survey on the same plan, open (`SurveyState::Open`).
    fn another_survey(&mut self) -> SurveyId {
        let first = self.repo.survey(self.survey).unwrap();
        let s = Survey {
            id: SurveyId::new(),
            state: SurveyState::Open,
            t_start: t(0),
            t_end: None,
            ..first
        };
        self.repo.insert_survey(&s).unwrap();
        s.id
    }

    /// `n` back-to-back 100 ms untracked rows at `f_hz` in `survey` from `start_ms`.
    fn untracked(&mut self, survey: SurveyId, f_hz: f64, start_ms: i64, n: i64) -> Vec<Detection> {
        let dets: Vec<Detection> = (0..n)
            .map(|i| {
                let a = start_ms + i * 100;
                Detection {
                    survey_id: survey,
                    ..self.det(f_hz, tr(a, a + 100), 12.0)
                }
            })
            .collect();
        self.repo.insert_detections(&dets).unwrap();
        dets
    }
}

/// T-904 review, blocker 1: a replay (or a run on a host whose clock is behind) writes into a
/// store that already holds newer rows. Its survey is open and its tracker holds tentative links
/// to its unlinked rows; ageing them from the store's newest row would delete them under the
/// tracker (and, before links tolerated a missing row, wedge track persistence for good). An
/// open survey ages from its own newest row; a closed one still ages from the store's.
#[test]
fn an_open_survey_ages_from_its_own_newest_row_not_the_stores() {
    let mut w = world();
    // Yesterday's live run, two hours "ahead" of the replay: closed, and with an old row.
    let live = w.survey;
    let old_live = w.untracked(live, 434.0e6, 0, 5);
    let _newest = w.untracked(live, 434.0e6, 7_200 * S, 5);
    w.repo
        .finish_survey(
            live,
            SurveyState::Closed,
            t(7_201 * S),
            &SurveySummary::default(),
        )
        .unwrap();
    // The replay: an open survey stamped with the recording's own (older) time, unlinked rows
    // the tracker has not confirmed yet, a minute of them.
    let replay = w.another_survey();
    let held = w.untracked(replay, 433.92e6, 1_000 * S, 600);
    let report = w
        .repo
        .prune_detections(&DetectionRetention::default(), || true)
        .unwrap();
    assert_eq!(
        w.remaining(&held).len(),
        600,
        "no row of the open replay survey is an hour older than its own newest: {report:?}"
    );
    assert!(
        w.remaining(&old_live).is_empty(),
        "the closed survey still ages from the store's newest row: {report:?}"
    );
    assert_eq!(report.deleted, 5);
    // A link the tracker writes afterwards lands: the row is still there.
    let (track, _) = w.track(433.92e6, 1_000 * S, 1);
    w.repo
        .link_detections_to_track(track, &[held[0].id], t(1_100 * S))
        .unwrap();
}

/// T-904 review, blocker 1 (c): a link to a detection that is no longer stored — aged out while
/// the tracker held it tentatively — is skipped; it never fails the write it is part of.
#[test]
fn a_link_to_a_pruned_detection_is_skipped_not_an_error() {
    let mut w = world();
    let (track, dets) = w.track(433.92e6, 0, 3);
    let gone = w.det(433.92e6, tr(300, 400), 10.0);
    w.repo
        .link_detections_to_track(track, &[gone.id, dets[0].id], t(500))
        .expect("a missing detection does not fail the link write");
    assert_eq!(w.repo.track_detections(track).unwrap().len(), 3);
}

/// T-904 review, blocker 2: rows no track links carry no verdict that they are one signal, so
/// they roll up by frequency. Two short bursts at opposite edges of a 20 MHz window, 2 s apart,
/// are two rollups, each its own time–frequency box — not one box spanning the window. Bursts at
/// one frequency inside the gap still share one.
#[test]
fn untracked_bursts_at_different_frequencies_roll_up_separately() {
    let mut w = world();
    let survey = w.survey;
    let lo = w.untracked(survey, 424.0e6, 0, 3);
    let hi = w.untracked(survey, 443.9e6, 2 * S, 3);
    let lo_again = w.untracked(survey, 424.0e6, 4 * S, 3);
    let _young = w.untracked(survey, 434.0e6, 7_200 * S, 1);
    let report = w.repo.prune_detections(&policy(60, 0, 4), || true).unwrap();
    assert_eq!(report.deleted, 9);
    let region = Region::new(FreqRange::new(400e6, 460e6), tr(0, 10 * S));
    let mut rollups = w.repo.detection_rollups_in_region(&region).unwrap();
    rollups.sort_by(|a, b| a.freq.lo_hz.total_cmp(&b.freq.lo_hz));
    assert_eq!(rollups.len(), 2, "{rollups:#?}");
    let (a, b) = (&rollups[0], &rollups[1]);
    assert!(a.freq.hi_hz < 425e6 && b.freq.lo_hz > 443e6, "{rollups:#?}");
    assert_eq!((a.detections, b.detections), (6, 3));
    assert_eq!(
        (a.time.start, a.time.end),
        (lo[0].time.start, lo_again[2].time.end)
    );
    assert_eq!(
        (b.time.start, b.time.end),
        (hi[0].time.start, hi[2].time.end)
    );
    assert!(a.track_id.is_none() && b.track_id.is_none());
}

// ---- T-913: follow-ups from T-904's review ----

/// A Track over `time` at `f_hz` (the fields the rollup path reads).
fn track_over(f_hz: f64, time: TimeRange, frames: u64) -> Track {
    Track {
        id: TrackId::new(),
        state: TrackState::Open,
        split_from: None,
        time,
        f_center_hz: f_hz,
        bandwidth_hz: 12e3,
        detection_count: frames,
        timing: TimingFeatures::default(),
        updated_at: time.end,
    }
}

/// T-913 (1): a rollup's time on air is the **union** of its members' intervals, never their sum.
/// Two co-timed rows — an FSK signal's two lobes in one frame, both linked to one track — are one
/// span of air, and the figure can never exceed the hull.
#[test]
fn co_timed_members_are_one_span_of_air_in_a_rollup() {
    let mut w = world();
    let dets = vec![
        w.det(433.92e6, tr(0, 100), 10.0),
        w.det(433.92e6, tr(100, 200), 10.0),
        // The other lobe of the same emission, in the same frame.
        w.det(433.93e6, tr(100, 200), 9.0),
        w.det(433.92e6, tr(200, 300), 10.0),
    ];
    w.repo.insert_detections(&dets).unwrap();
    let track = track_over(433.92e6, tr(0, 300), 4);
    w.repo.upsert_track(&track).unwrap();
    let ids: Vec<DetectionId> = dets.iter().map(|d| d.id).collect();
    w.repo
        .link_detections_to_track(track.id, &ids, t(300))
        .unwrap();
    // Move the watermark past the age.
    let _young = w.track(434.5e6, 3_600 * S, 1);
    w.repo
        .prune_detections(&policy(60, 0, 100), || true)
        .unwrap();

    let r = w.repo.track_rollups(track.id).unwrap();
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!((r[0].time, r[0].detections), (tr(0, 300), 4));
    // 300 ms of air from four 100 ms rows, not 400.
    assert_eq!(r[0].on_air_ns, 300 * NS, "{r:?}");
    assert!(r[0].on_air_ns <= r[0].time.duration_ns(), "{r:?}");
}

/// T-913 (3): a rollup summarises only rows that were **deleted** — a row the pass kept closes the
/// run it falls in, so no window is counted both as a surviving detection and inside a summary.
#[test]
fn a_kept_row_splits_the_rollup_it_would_have_fallen_inside() {
    let mut w = world();
    let (track, dets) = w.track(433.92e6, 0, 9);
    // Pin the middle row by linking it to an emitter directly (class 2).
    w.repo
        .record_sighting(
            &Sighting {
                source: LinkTarget::Detection(dets[4].id),
                seen: dets[4].time,
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
        .unwrap();
    let _young = w.track(434.5e6, 3_600 * S, 1);
    let report = w
        .repo
        .prune_detections(&policy(60, 0, 100), || true)
        .unwrap();
    assert_eq!((report.kept_pinned, report.deleted), (1, 8));

    let r = w.repo.track_rollups(track).unwrap();
    let kept = dets[4].time;
    assert_eq!(
        r.iter().map(|r| r.time).collect::<Vec<_>>(),
        vec![tr(0, 400), tr(500, 900)],
        "the kept row {kept:?} must fall between two rollups, not inside one"
    );
    assert!(
        r.iter()
            .all(|r| r.time.end <= kept.start || r.time.start >= kept.end),
        "{r:?} spans the surviving row {kept:?}"
    );
    assert_eq!(r.iter().map(|r| r.detections).sum::<u64>(), 8);
}

/// T-913 (3), across passes: the same holds when the earlier rollup was written by a previous
/// pass, which the in-memory barriers of *this* pass know nothing about.
#[test]
fn a_kept_row_splits_a_rollup_stored_by_an_earlier_pass() {
    let mut w = world();
    let (track, dets) = w.track(433.92e6, 0, 9);
    w.repo
        .record_sighting(
            &Sighting {
                source: LinkTarget::Detection(dets[4].id),
                seen: dets[4].time,
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
        .unwrap();
    let _young = w.track(434.5e6, 3_600 * S, 1);
    // Two passes: the first stops after one batch of five rows — the pinned row is the last of
    // them, so the second pass starts with no memory of it and must read the table to find it.
    let first = w
        .repo
        .prune_detections(&policy(60, 0, 5), || false)
        .unwrap();
    assert!(!first.complete, "{first:?}");
    assert_eq!((first.deleted, first.kept_pinned), (4, 1), "{first:?}");
    w.repo
        .prune_detections(&policy(60, 0, 100), || true)
        .unwrap();

    let r = w.repo.track_rollups(track).unwrap();
    let kept = dets[4].time;
    assert!(
        r.iter()
            .all(|r| r.time.end <= kept.start || r.time.start >= kept.end),
        "{r:?} spans the surviving row {kept:?}"
    );
    assert_eq!(r.iter().map(|r| r.detections).sum::<u64>(), 8);
}

/// T-913 (5): an explanation that cites a detection pins it, like any other reference by id. The
/// citation lives in an opaque JSON body, so it is written out to `explanation_detection`
/// (migration 0020) and the pin check reads that.
#[test]
fn a_detection_cited_by_an_explanation_is_pinned() {
    let mut w = world();
    let (_, dets) = w.track(433.92e6, 0, 6);
    let anomaly = Anomaly {
        id: AnomalyId::new(),
        kind: AnomalyKind::NoiseFloorRise,
        // Not the detection: that subject would pin it by itself.
        subject: AnomalySubject::Region,
        region: Region::new(FreqRange::new(433.9e6, 433.95e6), tr(0, 600)),
        score: 4.0,
        baseline_ref: None,
        t: t(600),
        detector_version: "test@1".into(),
    };
    w.repo.insert_anomaly(&anomaly).unwrap();
    w.repo
        .insert_explanation(&Explanation {
            id: ExplanationId::new(),
            anomaly_ref: anomaly.id,
            cause: Cause::Unexplained,
            correlation_type: CorrelationType::TimeCoincidence,
            score: 0.5,
            evidence: vec![Evidence::Detection { id: dets[2].id }],
            supersedes: None,
            provisional: false,
            rule_version: "test@1".into(),
            t: t(600),
        })
        .unwrap();

    let _young = w.track(434.5e6, 3_600 * S, 1);
    let report = w
        .repo
        .prune_detections(&policy(60, 0, 100), || true)
        .unwrap();
    assert_eq!(report.kept_pinned, 1, "{report:?}");
    assert_eq!(w.remaining(&dets), vec![dets[2].id]);
}

/// T-913 (11): a run that crashed left its survey open, and an open survey ages from its own
/// newest row — so its last hour is never aged out. The next run over the store aborts it at
/// start-up, and those rows then age from the store's watermark like any other closed survey's.
#[test]
fn a_survey_left_open_by_a_crashed_run_is_aborted_and_then_ages() {
    let dir = std::env::temp_dir().join(format!("hk-t913-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hackriff.db");
    let mut w = world_in(Repository::open(&path).unwrap());
    let crashed = w.survey;
    let (_, dets) = w.track(433.92e6, 0, 20);
    drop(w);

    // A second survey, hours later, in the same store: the next run.
    let mut next = world_in(Repository::open(&path).unwrap());
    assert_eq!(next.repo.abort_orphaned_surveys().unwrap(), 2);
    assert_eq!(
        next.repo.survey(crashed).unwrap().state,
        SurveyState::Aborted
    );
    // The aborted survey's own t_end is its newest detection, not a wall-clock instant.
    assert_eq!(next.repo.survey(crashed).unwrap().t_end, Some(t(2_000)));
    let (_, young) = next.track(434.5e6, 7_200 * S, 1);
    let report = next
        .repo
        .prune_detections(&policy(60, 0, 100), || true)
        .unwrap();
    assert_eq!(report.deleted, 20, "{report:?}");
    assert!(next.remaining(&dets).is_empty());
    assert_eq!(next.remaining(&young).len(), 1);
}

/// T-913 (12): a link whose detection is not stored is skipped — and counted, so a
/// drain-before-flush regression is visible instead of silently losing track membership.
#[test]
fn links_to_missing_detections_are_counted_not_silent() {
    let mut w = world();
    let (track, dets) = w.track(433.92e6, 0, 3);
    let ids: Vec<DetectionId> = dets.iter().map(|d| d.id).collect();
    // Re-linking what is already linked drops nothing: it is idempotent, not a loss.
    assert_eq!(
        w.repo.link_detections_to_track(track, &ids, t(300)).unwrap(),
        0
    );
    // Two never-stored detections, one stored: two dropped.
    let missing = [DetectionId::new(), ids[0], DetectionId::new()];
    assert_eq!(
        w.repo
            .link_detections_to_track(track, &missing, t(300))
            .unwrap(),
        2
    );
    assert_eq!(w.repo.track_detections(track).unwrap().len(), 3);
}

/// T-913 (8) / T-453, **timing tier** (`.config/nextest.toml`, `just timing`): what the DETECT
/// WRITER pays while a prune pass runs. The pass measures its *own* worst lock hold
/// ([`PruneReport::lock_ns_max`]) and its own wait behind the writer ([`PruneReport::wait_ns_max`]),
/// but neither says how long the writer waits behind *it* — and that is the figure that matters,
/// because the detection writer is what gates the capture thread's drain.
///
/// So: a second connection writes a frame of detections every 20 ms, exactly as the run's writer
/// does, while a pass prunes an hour of rows on the same file; each write's whole latency (the
/// `BEGIN IMMEDIATE` wait plus the insert itself) is recorded and the distribution printed.
///
/// **Measured 2026-09-25** on the dev box at load ~15 (four agents building), 24 000 rows pruned
/// in 240 batches while the writer wrote 8 rows every 20 ms: **354 writes, p50 3.08 ms, p95
/// 20.9 ms, worst 22.9 ms**, against the pass's own worst lock hold of 13.5 ms. So the writer
/// waits behind **one batch**, not behind the pass — which is what the batching is for — and the
/// 0.5–1.95 s outliers T-904 saw at 20 Msps are not this queue: nothing here approaches them even
/// on a loaded box. The bound asserted below is well above the measurement; the tier's point is
/// the number, not the threshold.
#[test]
fn a_detect_writer_waits_at_most_one_batch_behind_a_prune_pass() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let dir = std::env::temp_dir().join(format!("hk-t913-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hackriff.db");
    let mut w = world_in(Repository::open(&path).unwrap());
    // 20 minutes of the 2.4 Msps replay's rate (~20 rows/s), all past the age.
    for m in 0..20 {
        for k in 0..12 {
            let start = m * 60 * S + (k % 6) * 10 * S;
            w.track(433.1e6 + k as f64 * 100e3, start, 100);
        }
    }
    // One row an hour in, so every seeded row is past the age when the pass reads the watermark
    // (the writer's own live rows are younger than the age and are never candidates).
    w.track(434.9e6, 3_600 * S, 1);
    let (survey, prov) = (w.survey, w.prov);
    let stop = Arc::new(AtomicBool::new(false));
    let waits: Arc<Mutex<Vec<f64>>> = Arc::default();
    let writer = {
        let (stop, waits, path) = (Arc::clone(&stop), Arc::clone(&waits), path.clone());
        std::thread::spawn(move || {
            let mut repo = Repository::open(&path).unwrap();
            let mut frame = 0i64;
            while !stop.load(Ordering::Relaxed) {
                // A frame of live rows at the growing edge (well inside the age, so the pass
                // never looks at them).
                let at = 3_600 * S + frame * 100;
                let rows: Vec<Detection> = (0..8)
                    .map(|k| Detection {
                        id: DetectionId::new(),
                        survey_id: survey,
                        time: TimeRange::new(t(at), t(at + 100)),
                        f_center_hz: 433.1e6 + k as f64 * 100e3,
                        obw_hz: 12e3,
                        xdb_bandwidth_hz: None,
                        xdb_level_db: None,
                        snr_peak_db: 10.0,
                        snr_mean_db: 7.0,
                        peak_level_dbfs: -40.0,
                        peak_level_dbm: None,
                        sk: None,
                        clip_count: 0,
                        detector_version: "test@1".into(),
                        provenance_ref: prov,
                        flags: DetectionFlags::default(),
                    })
                    .collect();
                let asked = std::time::Instant::now();
                repo.insert_detections(&rows).unwrap();
                waits
                    .lock()
                    .unwrap()
                    .push(asked.elapsed().as_secs_f64() * 1e3);
                frame += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        })
    };

    let report = w
        .repo
        .prune_detections(&policy(20 * 60, 256, 100), || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            true
        })
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    let mut ms = waits.lock().unwrap().clone();
    assert!(ms.len() > 20, "the writer barely ran: {} writes", ms.len());
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms[((ms.len() as f64 - 1.0) * q) as usize];
    let (p50, p95, worst) = (at(0.5), at(0.95), ms[ms.len() - 1]);
    eprintln!(
        "detect writer behind a prune pass: {n} writes, p50 {p50:.2} ms, p95 {p95:.2} ms, \
         worst {worst:.1} ms; the pass's own worst lock hold {hold:.1} ms over {batches} batches",
        n = ms.len(),
        hold = report.lock_ns_max as f64 / 1e6,
        batches = report.batches
    );
    assert!(report.deleted > 10_000, "{report:?}");
    assert!(
        p95 < 250.0,
        "the detect writer's p95 write latency behind a prune pass was {p95:.1} ms \
         (worst {worst:.1} ms): {report:?}"
    );
}

/// T-913 (6), **timing tier**: what `CREATE INDEX idx_detection_t_end` (migration 0019) costs at
/// start-up on a store that ran unpruned, measured per row so the answer extrapolates.
///
/// The migration runs inside the transaction that opens the database, so the index build is on
/// the start-up path of the first run after the upgrade, and every page it writes goes through
/// the WAL. The question T-904's review asked was whether that is a problem worth moving off the
/// start-up path.
///
/// **Measured 2026-09-25** on the dev box at load ~15, 60 000 rows on a file database:
/// **41 ms, 0.68 µs/row, 1.8 MB of WAL (31 B/row)** — the WAL holds the index's own pages, not the
/// table's. Extrapolating at the staging device's measured 65–90 rows/s (T-904: ~7.8 M rows/day
/// unpruned): **≈ 5 s and ≈ 240 MB of WAL for a day, ≈ 37 s and ≈ 1.7 GB for a week** — slow, bounded,
/// paid exactly once per store, and the same work any later query would pay as a scan. That is
/// inside what a start-up may take, so the index stays on the start-up path deliberately, with
/// the number recorded rather than assumed; if it ever needs moving, this test is what says by
/// how much.
#[test]
fn the_retention_index_build_is_measured_per_row() {
    let dir = std::env::temp_dir().join(format!("hk-t913-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hackriff.db");
    let mut w = world_in(Repository::open(&path).unwrap());
    const ROWS: usize = 60_000;
    for chunk in 0..(ROWS / 500) {
        let rows: Vec<Detection> = (0..500)
            .map(|k| {
                let at = (chunk * 500 + k) as i64 * 20;
                w.det(433.1e6 + (k % 12) as f64 * 100e3, tr(at, at + 20), 10.0)
            })
            .collect();
        w.repo.insert_detections(&rows).unwrap();
    }
    w.repo
        .conn
        .execute_batch("DROP INDEX idx_detection_t_end")
        .unwrap();
    // TRUNCATE, not PASSIVE: the file is reset to zero, so what it holds afterwards is exactly
    // what the index build wrote.
    w.repo
        .conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    let wal_of = || {
        std::fs::metadata(format!("{}-wal", path.display()))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let wal_before = wal_of();
    let started = std::time::Instant::now();
    w.repo
        .conn
        .execute_batch("CREATE INDEX idx_detection_t_end ON detection (t_end)")
        .unwrap();
    let took = started.elapsed();
    let wal = wal_of().saturating_sub(wal_before);
    eprintln!(
        "idx_detection_t_end over {ROWS} rows: {:.0} ms ({:.2} µs/row), WAL grew {} kB \
         ({:.0} B/row)",
        took.as_secs_f64() * 1e3,
        took.as_secs_f64() * 1e6 / (ROWS as f64),
        wal / 1024,
        wal as f64 / (ROWS as f64)
    );
    // Well above the measurement: the number is the deliverable, the bound only catches an
    // order-of-magnitude regression (an index build that started scanning something else).
    assert!(
        took.as_secs_f64() * 1e6 / (ROWS as f64) < 50.0,
        "index build {:.0} ms over {ROWS} rows",
        took.as_secs_f64() * 1e3
    );
}
