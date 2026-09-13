//! AWARE-006 end to end (T-020): the T-023 `noise_floor_rise` synth (GNSS L1, +10 dB broadband
//! floor step), followed by a return to the original floor, replayed as ci8 through hk-dsp's STFT
//! and `NoiseFloorTracker` → `FloorAnomalies` (Anomaly opened on the NoiseLike Rise, closed on the
//! End) → a frozen, synthetic gpsjam extract (`tests/data/gpsjam-synthetic/`) → `Correlator`.
//!
//! Skips when `uv` is missing (`HK_E2E_REQUIRE_SYNTH=1` makes that a failure). No network.

use std::sync::OnceLock;

use hk_context::feeds::gpsjam::{GpsjamAdapter, SOURCE};
use hk_context::{
    Correlator, DirectoryFetcher, FeedCache, FloorAnomalies, FloorAnomalyConfig, Site, refresh, utc,
};
use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{
    EndReason, FloorChangeClass, FloorConfig, FloorEvent, FloorEventKind, NoiseFloorTracker,
};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_e2e::{Fixture, SynthRequest};
use hk_model::{
    AnomalyStatus, Cause, CorrelationType, Evidence, Repository, SampleTime, Timestamp,
};
use num_complex::Complex;

const K: usize = 10;
const DEVICE_CELL: &str = "2026-09-13/84194edffffffff";

struct Replayed {
    events: Vec<FloorEvent>,
    site: Site,
    t0: Timestamp,
    frame_period_s: f64,
}

fn provenance(center_hz: f64, sample_rate_hz: f64) -> hk_model::Provenance {
    serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:hk-context-aware-006",
        "tune": {"center_hz": center_hz, "sample_rate_hz": sample_rate_hz, "lna_db": 24.0,
                 "vga_db": 20.0, "amp_on": false, "bandwidth_hz": sample_rate_hz * 0.75},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .expect("provenance JSON")
}

fn generate(request: SynthRequest) -> Option<Fixture> {
    match request.generate() {
        Ok(out) => Some(out.fixture(0).unwrap()),
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP aware_006_e2e: {err}");
            None
        }
        Err(err) => panic!("AWARE-006: synthetic scenario generation failed: {err}"),
    }
}

fn ci8(fx: &Fixture) -> Vec<Complex<i8>> {
    fx.samples()
        .unwrap()
        .iter()
        .map(|s| Complex::new((s.re * 127.0).round() as i8, (s.im * 127.0).round() as i8))
        .collect()
}

/// Rise fixture (step at t0) then 2.5 s of the same scene without the step, as one stream.
fn replayed() -> Option<&'static Replayed> {
    static CELL: OnceLock<Option<Replayed>> = OnceLock::new();
    CELL.get_or_init(|| {
        let rise = generate(
            SynthRequest::new("noise_floor_rise")
                .seed(3)
                .param("duration_s", 3.0)
                .param("t0_s", 1.0),
        )?;
        let back = generate(
            SynthRequest::new("noise_floor_rise")
                .seed(3)
                .param("duration_s", 2.5)
                .param("t0_s", 2.0)
                .param("step_db", 0.0),
        )?;
        let fs = rise.sample_rate;
        let cap = &rise.meta.captures[0];
        let center = cap.frequency.expect("capture frequency");
        let start = utc::parse_utc(cap.datetime.as_deref().expect("capture datetime"))
            .expect("ISO-8601 capture datetime");
        let truth = rise.scenario().expect("scenario truth");
        let location = truth.get("location").expect("AWARE-006 truth location");
        let site = Site::new(
            location["lat"].as_f64().unwrap(),
            location["lon"].as_f64().unwrap(),
        );
        let t0 = utc::parse_utc(truth.str("t0_utc").expect("t0_utc")).unwrap();

        let mut samples = ci8(&rise);
        samples.extend(ci8(&back));
        let config = StftConfig::new(WelchConfig::new(1024), K);
        let frame_period_s = (K * config.welch.hop()) as f64 / fs;
        let mut stft = StftProcessor::new(config).unwrap();
        let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
        let prov = ProvenanceHandle::new(provenance(center, fs));
        let mut events = Vec::new();
        let n = samples.len() as u64;
        let mut at = 0u64;
        while at < n {
            let stop = (at + 65_536).min(n);
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: at,
                    host_time: start.saturating_add_nanos((at as f64 * 1e9 / fs) as i64),
                },
                provenance: prov.clone(),
                discontinuity: if at == 0 {
                    Discontinuity::STREAM_START
                } else {
                    Discontinuity::NONE
                },
                dropped_before: 0,
            };
            stft.push(
                InputInfo::from(&header),
                &samples[at as usize..stop as usize],
                |frame| {
                    tracker.update(frame, |e| events.push(e.clone()));
                },
            );
            at = stop;
        }
        Some(Replayed {
            events,
            site,
            t0,
            frame_period_s,
        })
    })
    .as_ref()
}

fn feed_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/gpsjam-synthetic")
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("aware-006-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs the floor events through the lifecycle; returns the repository and the one anomaly.
fn anomalies_from(r: &Replayed) -> (Repository, hk_model::AnomalyId) {
    let mut repo = Repository::open_in_memory().unwrap();
    let mut life = FloorAnomalies::new(FloorAnomalyConfig::new("synthetic:aware-006")).unwrap();
    let mut opened = Vec::new();
    for e in &r.events {
        opened.extend(life.on_floor_event(&mut repo, e).unwrap().opened);
    }
    assert_eq!(
        opened.len(),
        1,
        "AWARE-006: exactly one Anomaly: {:?}",
        r.events
    );
    (repo, opened[0])
}

fn ts(s: &str) -> Timestamp {
    utc::parse_utc(s).unwrap()
}

#[test]
fn aware_006_e2e_floor_rise_explained_by_frozen_gpsjam_cell() {
    let Some(r) = replayed() else { return };
    let rises: Vec<&FloorEvent> = r
        .events
        .iter()
        .filter(|e| e.kind == FloorEventKind::Rise)
        .collect();
    let ends: Vec<&FloorEvent> = r
        .events
        .iter()
        .filter(|e| e.kind == FloorEventKind::End)
        .collect();
    assert_eq!(rises.len(), 1, "AWARE-006: one Rise: {:?}", r.events);
    assert_eq!(
        rises[0].class,
        FloorChangeClass::NoiseLike,
        "AWARE-006: noise-like rise"
    );
    assert_eq!(
        ends.len(),
        1,
        "AWARE-006: one End after the floor returns: {:?}",
        r.events
    );
    assert_eq!(ends[0].episode, rises[0].episode);
    assert_eq!(ends[0].end_reason, Some(EndReason::Returned));

    let (mut repo, anomaly_id) = anomalies_from(r);
    let anomaly = repo.anomaly(anomaly_id).unwrap();
    assert_eq!(anomaly.kind, hk_model::AnomalyKind::NoiseFloorRise);
    assert!(anomaly.region.freq.lo_hz <= 1575.42e6 && anomaly.region.freq.hi_hz >= 1575.42e6);
    let onset_err_s =
        (anomaly.region.time.start.as_unix_nanos() - r.t0.as_unix_nanos()) as f64 * 1e-9;
    assert!(
        onset_err_s.abs() <= r.frame_period_s + 1e-6,
        "AWARE-006: anomaly onset {onset_err_s:+.4} s from t0"
    );
    let history = repo.anomaly_status_history(anomaly_id).unwrap();
    assert_eq!(
        history.iter().map(|h| h.status).collect::<Vec<_>>(),
        [AnomalyStatus::Open, AnomalyStatus::Resolved],
        "AWARE-006: the End closes the Anomaly"
    );

    // Backfill after the UTC day: the synthetic daily extract arrives, then correlation runs.
    let dir = scratch("positive");
    let cache = FeedCache::open(&dir).unwrap();
    let mut fetcher = DirectoryFetcher {
        dir: feed_dir(),
        fetched_at: ts("2026-09-14T01:00:00Z"),
    };
    let adapter = GpsjamAdapter::default();
    let ingest = refresh(
        &cache,
        &mut repo,
        &mut fetcher,
        &adapter,
        "2026-09-13",
        ts("2026-09-14T01:00:00Z"),
    )
    .unwrap();
    assert_eq!((ingest.events.len(), ingest.skipped), (3, 1));

    let now = ts("2026-09-14T02:00:00Z");
    let out = Correlator::default()
        .correlate(&mut repo, Some(&cache), anomaly_id, Some(&r.site), now)
        .unwrap();
    assert_eq!(
        out.written.len(),
        1,
        "AWARE-006: one matching cell: {:?}",
        out.candidates
    );
    let top = &repo.explanations_for_anomaly(anomaly_id).unwrap()[0];
    assert_eq!(top, &out.written[0]);
    let Cause::ExternalEvent { id } = top.cause else {
        panic!("AWARE-006: cause {:?}", top.cause)
    };
    let event = repo.external_event(id).unwrap();
    assert_eq!(event.source, SOURCE);
    assert_eq!(
        event.native_id, DEVICE_CELL,
        "AWARE-006: the cell containing the site"
    );
    assert_eq!(top.correlation_type, CorrelationType::TimeCoincidence);
    assert_eq!(
        top.evidence[0],
        Evidence::ExternalEvent {
            id,
            payload_hash: event.payload_hash().unwrap()
        },
        "AWARE-006: evidence pins the cached payload hash"
    );
    assert!(!top.provisional);
    assert!(
        (top.score - 0.9).abs() < 1e-9,
        "AWARE-006: score {}",
        top.score
    );
    eprintln!(
        "AWARE-006 e2e: anomaly {:.3}-{:.3} MHz onset {onset_err_s:+.4} s, score {:.1}; top explanation {} score {:.3}",
        anomaly.region.freq.lo_hz / 1e6,
        anomaly.region.freq.hi_hz / 1e6,
        anomaly.score,
        event.native_id,
        top.score
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn aware_006_e2e_no_explanation_when_the_cache_lacks_a_match() {
    let Some(r) = replayed() else { return };
    let now = ts("2026-09-14T02:00:00Z");
    let adapter = GpsjamAdapter::default();

    // Nothing cached.
    let (mut repo, id) = anomalies_from(r);
    let out = Correlator::default()
        .correlate(&mut repo, None, id, Some(&r.site), now)
        .unwrap();
    assert!(out.written.is_empty(), "AWARE-006: empty cache");

    // The extract is cached, but the device is elsewhere.
    let dir = scratch("negative");
    let cache = FeedCache::open(&dir).unwrap();
    let mut fetcher = DirectoryFetcher {
        dir: feed_dir(),
        fetched_at: ts("2026-09-14T01:00:00Z"),
    };
    refresh(&cache, &mut repo, &mut fetcher, &adapter, "2026-09-13", now).unwrap();
    let out = Correlator::default()
        .correlate(
            &mut repo,
            Some(&cache),
            id,
            Some(&Site::new(46.5, 10.0)),
            now,
        )
        .unwrap();
    assert!(out.written.is_empty(), "AWARE-006: other location");

    // Only a different day is cached (same cells, dated two days earlier).
    let (mut repo, id) = anomalies_from(r);
    let other_day = scratch("other-day");
    std::fs::copy(
        feed_dir().join("2026-09-13-h3_4.csv"),
        other_day.join("2026-09-11-h3_4.csv"),
    )
    .unwrap();
    let cache = FeedCache::open(&other_day).unwrap();
    let mut fetcher = DirectoryFetcher {
        dir: other_day.clone(),
        fetched_at: ts("2026-09-12T01:00:00Z"),
    };
    refresh(&cache, &mut repo, &mut fetcher, &adapter, "2026-09-11", now).unwrap();
    let out = Correlator::default()
        .correlate(&mut repo, Some(&cache), id, Some(&r.site), now)
        .unwrap();
    assert!(
        out.written.is_empty() && repo.explanations_for_anomaly(id).unwrap().is_empty(),
        "AWARE-006: other day"
    );
    std::fs::remove_dir_all(dir).ok();
    std::fs::remove_dir_all(other_day).ok();
}
