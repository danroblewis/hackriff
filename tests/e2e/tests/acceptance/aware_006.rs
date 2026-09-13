//! AWARE-006 (T4): GNSS-jamming attack-map entry. The `noise_floor_rise` synth (GNSS L1 at
//! 1575.42 MHz, 2 Msps, a +10 dB broadband floor step at t0 = 1 s, site 52.2 N 0.12 E) replayed
//! through the composed pipeline, whose detect reader runs the floor-anomaly lifecycle and the
//! correlator against the offline feed cache (`PipelineConfig::feeds_dir`, site from
//! `ScanPlan.extra.pipeline.site`). The frozen cache is T-020's synthetic gpsjam extract
//! (`crates/hk-context/tests/data/gpsjam-synthetic/2026-09-13-h3_4.csv`), ingested before the run
//! as the device would have synced it.
//!
//! Positive: one `noise-floor-rise` Anomaly and a top Explanation (time-coincidence/geometry)
//! citing the cell that contains the site. Negative: the same scene with an empty cache, and with
//! the extract cached but the device elsewhere, yields the Anomaly and **no** Explanation.

use hk_context::feeds::gpsjam::{GpsjamAdapter, SOURCE};
use hk_context::{DirectoryFetcher, FeedCache, refresh, utc};
use hk_e2e::{Fixture, SynthRequest};
use hk_model::{AnomalyKind, Cause, CorrelationType, FreqRange, Region, Repository};
use serde_json::json;

use crate::common::*;

const AWARE_006: &str = "AWARE-006";
const SITE_CELL: &str = "2026-09-13/84194edffffffff";

fn scenario() -> Option<Fixture> {
    match SynthRequest::new("noise_floor_rise")
        .seed(3)
        .param("duration_s", 3.0)
        .param("t0_s", 1.0)
        .generate()
    {
        Ok(out) => Some(out.fixture(0).unwrap()),
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {AWARE_006}: {e}");
            None
        }
        Err(e) => panic!("[{AWARE_006}] synthetic scenario generation failed: {e}"),
    }
}

fn site_of(fx: &Fixture) -> [f64; 2] {
    let loc = &fx.scenario().unwrap().value["location"];
    [loc["lat"].as_f64().unwrap(), loc["lon"].as_f64().unwrap()]
}

/// Replays the scene with the feed cache in the data directory (`extract`: the frozen gpsjam day
/// is ingested first) and the device at `site`. Returns the run's repository and anomaly.
fn run(
    fx: &Fixture,
    extract: bool,
    site: [f64; 2],
    tag: &str,
) -> (TempDir, Repository, hk_model::Anomaly) {
    let dir = TempDir::new(tag);
    if extract {
        let synced = utc::parse_utc("2026-09-13T11:30:00Z").unwrap();
        let mut repo = repo(&dir.0);
        let cache = FeedCache::open(&dir.0).unwrap();
        let mut fetcher = DirectoryFetcher {
            dir: hk_e2e::paths::repo_root().join("crates/hk-context/tests/data/gpsjam-synthetic"),
            fetched_at: synced,
        };
        let report = refresh(
            &cache,
            &mut repo,
            &mut fetcher,
            &GpsjamAdapter::default(),
            "2026-09-13",
            synced,
        )
        .unwrap();
        assert_eq!(
            report.events.len(),
            3,
            "[{AWARE_006}] frozen extract events"
        );
    }
    let (mut cfg, replay) = replay_config(
        &dir.0,
        &fx.meta_path,
        json!({ "pipeline": { "site": site } }),
        hk_core::Pacing::Unpaced,
    );
    cfg.feeds_dir = Some(dir.0.clone());
    let s = finish(start(cfg, replay));
    assert_eq!(s.always_on_lost_samples, 0);
    let repo = repo(&dir.0);
    let anomalies: Vec<_> = repo
        .anomalies_in_region(&Region::new(FreqRange::centered(1575.42e6, 2e6), ever()))
        .unwrap()
        .into_iter()
        .filter(|a| a.kind == AnomalyKind::NoiseFloorRise)
        .collect();
    eprintln!(
        "[{AWARE_006}] {tag}: {} anomalies opened, {} explanations (counters), {} noise-floor-rise rows",
        s.counter("/detect/anomalies_opened"),
        s.counter("/detect/explanations"),
        anomalies.len()
    );
    assert_eq!(
        anomalies.len(),
        1,
        "[{AWARE_006}] {tag}: exactly one noise-floor-rise Anomaly: {anomalies:?}"
    );
    let a = anomalies.into_iter().next().unwrap();
    let t0 = utc::parse_utc(fx.scenario().unwrap().str("t0_utc").unwrap()).unwrap();
    let onset_s = (a.region.time.start.as_unix_nanos() - t0.as_unix_nanos()) as f64 * 1e-9;
    eprintln!(
        "[{AWARE_006}] {tag}: anomaly {:.3}-{:.3} MHz, onset {onset_s:+.4} s from t0",
        a.region.freq.lo_hz / 1e6,
        a.region.freq.hi_hz / 1e6
    );
    assert!(
        a.region.freq.lo_hz <= 1575.42e6 && a.region.freq.hi_hz >= 1575.42e6,
        "[{AWARE_006}] anomaly covers L1"
    );
    assert!(onset_s.abs() <= 0.1, "[{AWARE_006}] onset {onset_s:+.4} s");
    (dir, repo, a)
}

#[test]
fn aware_006_floor_rise_explained_by_frozen_gpsjam_event() {
    let Some(fx) = scenario() else { return };
    let (_dir, repo, a) = run(&fx, true, site_of(&fx), "a006pos");
    let ex = repo.explanations_for_anomaly(a.id).unwrap();
    assert!(!ex.is_empty(), "[{AWARE_006}] no Explanation");
    let top = &ex[0];
    assert!(
        matches!(
            top.correlation_type,
            CorrelationType::TimeCoincidence | CorrelationType::Geometry
        ),
        "[{AWARE_006}] correlation type {:?}",
        top.correlation_type
    );
    let Cause::ExternalEvent { id } = top.cause else {
        panic!("[{AWARE_006}] cause {:?}", top.cause)
    };
    let event = repo.external_event(id).unwrap();
    eprintln!(
        "[{AWARE_006}] top explanation: {} {} ({:?}, score {:.3}); {} explanations",
        event.source,
        event.native_id,
        top.correlation_type,
        top.score,
        ex.len()
    );
    assert_eq!(event.source, SOURCE);
    assert_eq!(
        event.native_id, SITE_CELL,
        "[{AWARE_006}] the gpsjam cell containing the site ranks first"
    );
}

#[test]
fn aware_006_no_explanation_without_a_matching_cached_event() {
    let Some(fx) = scenario() else { return };
    let (_d1, repo, a) = run(&fx, false, site_of(&fx), "a006empty");
    assert!(
        repo.explanations_for_anomaly(a.id).unwrap().is_empty(),
        "[{AWARE_006}] Explanation from an empty cache"
    );
    let (_d2, repo, a) = run(&fx, true, [46.5, 10.0], "a006far");
    assert!(
        repo.explanations_for_anomaly(a.id).unwrap().is_empty(),
        "[{AWARE_006}] Explanation for a device outside every jammed cell"
    );
}
