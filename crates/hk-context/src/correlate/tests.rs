//! AWARE-006 correlation tests on a frozen gpsjam cache (no network).

use hk_model::{AnomalySubject, Geo, Region};
use serde_json::json;

use super::*;
use crate::feeds::gpsjam::{GpsjamAdapter, SOURCE};
use crate::feeds::{FeedAdapter, FeedCache, OfflineFetcher, ingest_snapshot, refresh};
use crate::utc::parse_utc;

const BODY: &str = "hex,count_good_aircraft,count_bad_aircraft\n\
                    84194edffffffff,40,12\n\
                    8419453ffffffff,30,2\n\
                    84261b5ffffffff,50,30\n";
const DEVICE_CELL: &str = "2026-09-13/84194edffffffff";

fn ts(s: &str) -> Timestamp {
    parse_utc(s).unwrap()
}

fn fetched() -> Timestamp {
    ts("2026-09-14T01:00:00Z")
}

fn now() -> Timestamp {
    ts("2026-09-14T02:00:00Z")
}

fn site() -> Site {
    Site::new(52.2, 0.12)
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hk-context-correlate-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cache_events(repo: &mut Repository, body: &str) {
    let parsed = GpsjamAdapter::default()
        .parse("2026-09-13", body, fetched())
        .unwrap();
    for e in &parsed.events {
        repo.upsert_external_event(e).unwrap();
    }
}

fn anomaly_at(repo: &mut Repository, onset: &str, lo_mhz: f64, hi_mhz: f64) -> Anomaly {
    let start = ts(onset);
    let a = Anomaly {
        id: AnomalyId::new(),
        kind: AnomalyKind::NoiseFloorRise,
        subject: AnomalySubject::Region,
        region: Region::new(
            FreqRange::new(lo_mhz * 1e6, hi_mhz * 1e6),
            TimeRange::new(start, start.saturating_add_nanos(1_000_000_000)),
        ),
        score: 50.0,
        baseline_ref: None,
        t: start,
        detector_version: "test".into(),
    };
    repo.insert_anomaly(&a).unwrap();
    a
}

fn l1(repo: &mut Repository) -> Anomaly {
    anomaly_at(repo, "2026-09-13T12:00:01Z", 1574.42, 1576.42)
}

fn value(e: &Explanation, name: &str) -> f64 {
    e.evidence
        .iter()
        .find_map(|ev| match ev {
            Evidence::Value { name: n, value } if n == name => Some(*value),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no evidence value {name}"))
}

fn correlate(repo: &mut Repository, a: &Anomaly, site: Option<&Site>) -> CorrelationOutcome {
    Correlator::default()
        .correlate(repo, None, a.id, site, now())
        .unwrap()
}

#[test]
fn aware_006_containing_gpsjam_cell_is_the_top_explanation() {
    let mut repo = Repository::open_in_memory().unwrap();
    cache_events(&mut repo, BODY);
    let a = l1(&mut repo);
    let out = correlate(&mut repo, &a, Some(&site()));
    assert_eq!(
        out.written.len(),
        1,
        "only the cell containing the site: {:?}",
        out.candidates
    );
    let top = &repo.explanations_for_anomaly(a.id).unwrap()[0];
    assert_eq!(top, &out.written[0]);
    let event = repo
        .external_events_overlapping(&a.region.time, Some(SOURCE))
        .unwrap()
        .into_iter()
        .find(|e| e.native_id == DEVICE_CELL)
        .unwrap();
    assert_eq!(top.cause, Cause::ExternalEvent { id: event.id });
    assert_eq!(top.correlation_type, CorrelationType::TimeCoincidence);
    assert_eq!(
        top.evidence[0],
        Evidence::ExternalEvent {
            id: event.id,
            payload_hash: event.payload_hash().unwrap()
        }
    );
    assert!(!top.provisional);
    assert_eq!(top.rule_version, RULE_VERSION);
    assert_eq!(top.t, now());
    // 21.2 % bad → magnitude 1; inside the day, the cell and L1 → score = prior.
    assert!((top.score - 0.9).abs() < 1e-12, "{}", top.score);
    assert_eq!(value(top, "distance_km"), 0.0);
    assert_eq!(value(top, "time_gap_s"), 0.0);
    assert_eq!(value(top, "band_overlap"), 1.0);
    assert_eq!(value(top, "source_stale"), 0.0);
    assert_eq!(value(top, "cache_age_s"), 3600.0);
}

#[test]
fn aware_006_no_explanation_without_a_match() {
    // Empty cache.
    let mut repo = Repository::open_in_memory().unwrap();
    let a = l1(&mut repo);
    let out = correlate(&mut repo, &a, Some(&site()));
    assert!(out.written.is_empty() && out.candidates.is_empty());

    let mut repo = Repository::open_in_memory().unwrap();
    cache_events(&mut repo, BODY);
    // Another location (Alps, far from every cached cell), and no site at all.
    let a = l1(&mut repo);
    assert!(
        correlate(&mut repo, &a, Some(&Site::new(46.5, 10.0)))
            .written
            .is_empty()
    );
    assert!(
        correlate(&mut repo, &a, None).written.is_empty(),
        "no site, no geometry claim"
    );
    // Another day (beyond the 2 h lag window).
    let later = anomaly_at(&mut repo, "2026-09-15T12:00:00Z", 1574.42, 1576.42);
    assert!(
        correlate(&mut repo, &later, Some(&site()))
            .written
            .is_empty()
    );
    // A non-GNSS band at the right time and place.
    let ism = anomaly_at(&mut repo, "2026-09-13T12:00:01Z", 433.0, 435.0);
    assert!(correlate(&mut repo, &ism, Some(&site())).written.is_empty());
    // Not a floor rise.
    let mut novelty = l1(&mut repo);
    novelty.kind = AnomalyKind::Novelty;
    let events = repo
        .external_events_overlapping(&novelty.region.time, None)
        .unwrap();
    assert!(
        Correlator::default()
            .rank(&novelty, Some(&site()), &events, &BTreeMap::new(), now())
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.explanations_in_region(&Region::new(
            FreqRange::new(0.0, 1e10),
            TimeRange::new(Timestamp::from_unix_nanos(0), now())
        ))
        .unwrap()
        .is_empty()
    );
}

#[test]
fn aware_006_lag_window_and_distance_decay_are_documented_linear_factors() {
    let mut repo = Repository::open_in_memory().unwrap();
    cache_events(&mut repo, BODY);
    // One hour after the UTC day ends: s_time = 1 − 3600/7200 = 0.5 (anomaly span starts at
    // 01:00:00, the day's closed span ends 1 ns before midnight).
    let a = anomaly_at(&mut repo, "2026-09-14T01:00:00Z", 1575.0, 1576.0);
    let out = correlate(&mut repo, &a, Some(&site()));
    assert_eq!(out.written.len(), 1);
    let e = &out.written[0];
    assert!((value(e, "score_time") - 0.5).abs() < 1e-9);
    assert!((e.score - 0.45).abs() < 1e-9);
    // Partial band overlap: half the anomaly band is above L1's upper edge (1610 MHz).
    let b = anomaly_at(&mut repo, "2026-09-13T12:00:00Z", 1608.0, 1612.0);
    let out = correlate(&mut repo, &b, Some(&site()));
    assert!((value(&out.written[0], "band_overlap") - 0.5).abs() < 1e-9);
    // The yellow cell (3.1 % → magnitude 0.57) ~150 km away stays out at R = 50 km; widening R
    // lets it in, ranked below the containing red cell.
    let wide = Correlator::new(CorrelatorConfig {
        geo_radius_km: 400.0,
        ..CorrelatorConfig::default()
    })
    .unwrap();
    let c = l1(&mut repo);
    let events = repo
        .external_events_overlapping(&wide.query_window(&c), None)
        .unwrap();
    let ranked = wide
        .rank(&c, Some(&site()), &events, &BTreeMap::new(), now())
        .unwrap();
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].event.native_id, DEVICE_CELL);
    assert!(ranked[1].distance_km > 100.0 && ranked[1].score < ranked[0].score);
}

#[test]
fn aware_006_recorrelation_is_idempotent() {
    let mut repo = Repository::open_in_memory().unwrap();
    cache_events(&mut repo, BODY);
    let a = l1(&mut repo);
    let first = correlate(&mut repo, &a, Some(&site()));
    let second = correlate(&mut repo, &a, Some(&site()));
    assert!(second.written.is_empty());
    assert_eq!(second.unchanged, [first.written[0].id]);
    // Replaying the same snapshot changes nothing either.
    cache_events(&mut repo, BODY);
    assert!(correlate(&mut repo, &a, Some(&site())).written.is_empty());
    assert_eq!(repo.explanations_for_anomaly(a.id).unwrap().len(), 1);
}

#[test]
fn aware_006_stale_cache_is_flagged_provisional_then_refreshed() {
    let mut repo = Repository::open_in_memory().unwrap();
    cache_events(&mut repo, BODY);
    let a = l1(&mut repo);
    // Evaluated three days after the fetch: past valid_until (fetch + 24 h).
    let late = ts("2026-09-17T01:00:00Z");
    let out = Correlator::default()
        .correlate(&mut repo, None, a.id, Some(&site()), late)
        .unwrap();
    let e = &out.written[0];
    assert!(e.provisional);
    assert_eq!(value(e, "source_stale"), 1.0);
    assert_eq!(value(e, "cache_age_s"), 3.0 * 86_400.0);
    assert!(
        (e.score - 0.9).abs() < 1e-12,
        "stale data still correlates at full score"
    );

    // A fresh fetch of the same payload (same hash) clears the flag: a superseding row.
    let parsed = GpsjamAdapter::default()
        .parse("2026-09-13", BODY, ts("2026-09-17T00:30:00Z"))
        .unwrap();
    for ev in &parsed.events {
        repo.upsert_external_event(ev).unwrap();
    }
    let out2 = Correlator::default()
        .correlate(&mut repo, None, a.id, Some(&site()), late)
        .unwrap();
    assert_eq!(out2.written.len(), 1);
    assert!(!out2.written[0].provisional);
    assert_eq!(out2.written[0].supersedes, Some(e.id));
    assert!(
        out2.stale_evidence.is_empty(),
        "same payload hash is not stale evidence"
    );
}

#[test]
fn aware_006_failed_refresh_marks_feed_stale_and_correlation_provisional() {
    let dir = temp_dir("stale-feed");
    let cache = FeedCache::open(&dir).unwrap();
    let mut repo = Repository::open_in_memory().unwrap();
    ingest_snapshot(
        &cache,
        &mut repo,
        &GpsjamAdapter::default(),
        "2026-09-13",
        BODY,
        fetched(),
    )
    .unwrap();
    let a = l1(&mut repo);
    let fresh = Correlator::default()
        .correlate(&mut repo, Some(&cache), a.id, Some(&site()), now())
        .unwrap();
    assert!(!fresh.written[0].provisional);
    let err = refresh(
        &cache,
        &mut repo,
        &mut OfflineFetcher,
        &GpsjamAdapter::default(),
        "2026-09-14",
        now(),
    );
    assert!(matches!(err, Err(FeedError::Fetch { .. })));
    assert!(cache.state(SOURCE).unwrap().unwrap().stale);
    let out = Correlator::default()
        .correlate(&mut repo, Some(&cache), a.id, Some(&site()), now())
        .unwrap();
    assert_eq!(
        out.written.len(),
        1,
        "offline still correlates from the cache"
    );
    assert!(out.written[0].provisional);
    assert_eq!(out.written[0].supersedes, Some(fresh.written[0].id));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn aware_006_revised_payload_surfaces_stale_evidence_and_recorrelates() {
    let mut repo = Repository::open_in_memory().unwrap();
    cache_events(&mut repo, BODY);
    let a = l1(&mut repo);
    let c = Correlator::default();
    let first = c
        .correlate(&mut repo, None, a.id, Some(&site()), now())
        .unwrap();
    let old = first.written[0].clone();
    let events = repo
        .external_events_overlapping(&c.query_window(&a), None)
        .unwrap();
    let ranked_before = c
        .rank(&a, Some(&site()), &events, &BTreeMap::new(), now())
        .unwrap();

    // The feed revises the device cell (still red, different counts).
    let revised = BODY.replace("84194edffffffff,40,12", "84194edffffffff,38,20");
    cache_events(&mut repo, &revised);

    // Writing candidates ranked before the revision is refused by the repository.
    let refused = c.write(&mut repo, &a, &ranked_before[..1], Some(&site()), now());
    let event_id = ranked_before[0].event.id;
    assert!(matches!(
        refused,
        Err(CorrelateError::Repo(RepoError::StaleEvidence { event, .. })) if event == event_id
    ));

    // Correlating again reports the old pin as stale and supersedes it with the current payload.
    let out = c
        .correlate(&mut repo, None, a.id, Some(&site()), now())
        .unwrap();
    let current = repo
        .external_event(event_id)
        .unwrap()
        .payload_hash()
        .unwrap();
    assert_eq!(
        out.stale_evidence,
        [StaleEvidence {
            explanation: Some(old.id),
            event: event_id,
            pinned: ranked_before[0].payload_hash,
            current
        }]
    );
    assert_eq!(out.written.len(), 1);
    let new = &out.written[0];
    assert_eq!(new.supersedes, Some(old.id));
    assert_eq!(
        new.evidence[0],
        Evidence::ExternalEvent {
            id: event_id,
            payload_hash: current
        }
    );
    assert_eq!(out.attempts, 1);
    // And that is now settled.
    let again = c
        .correlate(&mut repo, None, a.id, Some(&site()), now())
        .unwrap();
    assert!(again.written.is_empty() && again.stale_evidence.is_empty());
}

#[test]
fn aware_006_deterministic_on_a_frozen_cache() {
    let run = || {
        let mut repo = Repository::open_in_memory().unwrap();
        cache_events(&mut repo, BODY);
        let a = l1(&mut repo);
        let c = Correlator::new(CorrelatorConfig {
            geo_radius_km: 400.0,
            ..CorrelatorConfig::default()
        })
        .unwrap();
        let out = c
            .correlate(&mut repo, None, a.id, Some(&site()), now())
            .unwrap();
        out.written
            .into_iter()
            .map(|e| {
                (
                    e.cause,
                    e.correlation_type,
                    e.score.to_bits(),
                    e.evidence.into_iter().skip(2).collect::<Vec<_>>(),
                    e.provisional,
                    e.rule_version,
                    e.t,
                )
            })
            .collect::<Vec<_>>()
    };
    let (a, b) = (run(), run());
    assert_eq!(a.len(), 2);
    assert_eq!(
        a, b,
        "identical causes (ids derive from the natural key), scores and evidence"
    );
}

#[test]
fn ties_rank_by_event_start_then_native_id_and_config_is_validated() {
    let mk = |native: &str| ExternalEvent {
        id: crate::feeds::gpsjam::event_id(SOURCE, native),
        source: SOURCE.into(),
        native_id: native.into(),
        event_type: "gnss-interference-cell".into(),
        time: crate::utc::day_range("2026-09-13").unwrap(),
        geo: Geo::Global,
        freq: vec![FreqRange::new(1559e6, 1610e6)],
        payload: json!({"percent_bad": 50.0}),
        fetched_at: fetched(),
        valid_until: None,
    };
    let mut repo = Repository::open_in_memory().unwrap();
    let a = l1(&mut repo);
    let events = [mk("b"), mk("a")];
    let ranked = Correlator::default()
        .rank(&a, None, &events, &BTreeMap::new(), now())
        .unwrap();
    assert_eq!(
        ranked
            .iter()
            .map(|c| c.event.native_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(
        Correlator::new(CorrelatorConfig {
            min_score: 0.0,
            ..CorrelatorConfig::default()
        })
        .is_err()
    );
    assert!(
        Correlator::new(CorrelatorConfig {
            time_window_s: f64::NAN,
            ..CorrelatorConfig::default()
        })
        .is_err()
    );
    assert!(
        Correlator::default()
            .rank(
                &a,
                Some(&Site::new(95.0, 0.0)),
                &events,
                &BTreeMap::new(),
                now()
            )
            .is_err()
    );
}
