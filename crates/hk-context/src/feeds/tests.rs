//! Feed-cache tests: offline-first ingest, idempotence, stale flags, restart from disk.

use super::gpsjam::{GpsjamAdapter, SOURCE};
use super::*;
use crate::utc::{day_range, parse_utc};

const BODY: &str = "hex,count_good_aircraft,count_bad_aircraft\n\
                    84194edffffffff,40,12\n\
                    840d9edffffffff,20,0\n";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hk-context-feeds-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn ts(s: &str) -> Timestamp {
    parse_utc(s).unwrap()
}

#[test]
fn aware_006_ingest_is_offline_first_idempotent_and_survives_reopen() {
    let dir = temp_dir("ingest");
    let adapter = GpsjamAdapter::default();
    let mut repo = Repository::open_in_memory().unwrap();
    let cache = FeedCache::open(&dir).unwrap();
    assert_eq!(cache.state(SOURCE).unwrap(), None);
    let fetched = ts("2026-09-14T01:00:00Z");
    let r1 = ingest_snapshot(&cache, &mut repo, &adapter, "2026-09-13", BODY, fetched).unwrap();
    assert_eq!((r1.events.len(), r1.skipped), (1, 1));
    let r2 = ingest_snapshot(&cache, &mut repo, &adapter, "2026-09-13", BODY, fetched).unwrap();
    assert_eq!(r1, r2, "same ids, hashes and snapshot");
    let raw_dir = dir.join("context/feeds/gpsjam/raw/2026-09-13");
    assert_eq!(
        fs::read_dir(&raw_dir).unwrap().count(),
        1,
        "content-addressed raw file"
    );

    // Reopen (process restart): state and snapshot come back from disk.
    let cache = FeedCache::open(&dir).unwrap();
    let state = cache.state(SOURCE).unwrap().unwrap();
    assert_eq!(state.fetched_at, Some(fetched));
    assert_eq!(
        state.valid_until,
        Some(fetched.saturating_add_nanos(86_400_000_000_000))
    );
    assert!(!state.is_stale(ts("2026-09-14T12:00:00Z")));
    assert!(
        state.is_stale(ts("2026-09-15T01:00:00.000000001Z")),
        "past the validity window"
    );
    assert_eq!(state.cache_age_s(ts("2026-09-14T02:00:00Z")), Some(3600.0));
    assert!(state.covers(&day_range("2026-09-13").unwrap()));
    assert!(
        !state.covers(&day_range("2026-09-14").unwrap()),
        "no data is not no event"
    );
    assert_eq!(
        cache.snapshot(SOURCE, "2026-09-13").unwrap().as_deref(),
        Some(BODY)
    );
    assert_eq!(state.parser_version, gpsjam::PARSER_VERSION);

    // A revised snapshot for the same key keeps event ids, changes the payload hash, and sits
    // next to the first raw file.
    let revised = BODY.replace(",40,12", ",38,20");
    let r3 = ingest_snapshot(
        &cache,
        &mut repo,
        &adapter,
        "2026-09-13",
        &revised,
        ts("2026-09-14T03:00:00Z"),
    )
    .unwrap();
    assert_eq!(r3.events[0].0, r1.events[0].0);
    assert_ne!(r3.events[0].1, r1.events[0].1);
    assert_eq!(fs::read_dir(&raw_dir).unwrap().count(), 2);
    assert_eq!(
        cache.snapshot(SOURCE, "2026-09-13").unwrap().as_deref(),
        Some(revised.as_str())
    );
    // An older snapshot imported later does not move the validity window back.
    ingest_snapshot(
        &cache,
        &mut repo,
        &adapter,
        "2026-09-12",
        BODY,
        ts("2026-09-13T01:00:00Z"),
    )
    .unwrap();
    let state = cache.state(SOURCE).unwrap().unwrap();
    assert_eq!(state.fetched_at, Some(ts("2026-09-14T03:00:00Z")));
    assert_eq!(
        state.coverage.len(),
        1,
        "adjacent days merge: {:?}",
        state.coverage
    );
    fs::remove_dir_all(dir).ok();
}

#[test]
fn aware_006_parse_and_fetch_failures_mark_stale_but_keep_the_cache() {
    let dir = temp_dir("fail");
    let adapter = GpsjamAdapter::default();
    let mut repo = Repository::open_in_memory().unwrap();
    let cache = FeedCache::open(&dir).unwrap();
    let fetched = ts("2026-09-14T01:00:00Z");
    ingest_snapshot(&cache, &mut repo, &adapter, "2026-09-13", BODY, fetched).unwrap();

    let bad = ingest_snapshot(
        &cache,
        &mut repo,
        &adapter,
        "2026-09-14",
        "hex,good\n",
        fetched,
    );
    assert!(matches!(bad, Err(FeedError::Parse { ref error, .. }) if error.line == 1));
    let state = cache.state(SOURCE).unwrap().unwrap();
    assert!(state.stale && state.is_stale(fetched));
    assert!(state.last_error.as_deref().unwrap().contains("2026-09-14"));
    assert!(!state.snapshots.contains_key("2026-09-14"));

    // Fetch failures: offline, then a directory without that day.
    let now = ts("2026-09-15T02:00:00Z");
    assert!(matches!(
        refresh(
            &cache,
            &mut repo,
            &mut OfflineFetcher,
            &adapter,
            "2026-09-14",
            now
        ),
        Err(FeedError::Fetch {
            error: FetchError::Offline,
            ..
        })
    ));
    let mut dir_fetcher = DirectoryFetcher {
        dir: dir.join("none"),
        fetched_at: now,
    };
    assert!(matches!(
        refresh(
            &cache,
            &mut repo,
            &mut dir_fetcher,
            &adapter,
            "2026-09-14",
            now
        ),
        Err(FeedError::Fetch {
            error: FetchError::NotFound(_),
            ..
        })
    ));
    let state = cache.state(SOURCE).unwrap().unwrap();
    assert_eq!(state.last_attempt, Some(now));
    assert!(state.stale);
    // The cached events are untouched.
    let events = repo
        .external_events_overlapping(&day_range("2026-09-13").unwrap(), Some(SOURCE))
        .unwrap();
    assert_eq!(events.len(), 1);

    // A successful refresh from a frozen directory clears the flag.
    fs::create_dir_all(dir.join("frozen")).unwrap();
    fs::write(
        dir.join("frozen").join(adapter.file_name("2026-09-14")),
        BODY,
    )
    .unwrap();
    let mut frozen = DirectoryFetcher {
        dir: dir.join("frozen"),
        fetched_at: now,
    };
    refresh(&cache, &mut repo, &mut frozen, &adapter, "2026-09-14", now).unwrap();
    let state = cache.state(SOURCE).unwrap().unwrap();
    assert!(!state.stale && state.last_error.is_none());
    assert!(!state.is_stale(now));
    fs::remove_dir_all(dir).ok();
}

#[test]
fn names_coverage_and_query_order() {
    let dir = temp_dir("names");
    let cache = FeedCache::open(&dir).unwrap();
    for bad in ["", "../x", ".hidden", "a/b", "a b"] {
        assert!(
            matches!(cache.state(bad), Err(FeedError::InvalidName(_))),
            "{bad:?}"
        );
        assert!(cache.store_snapshot(SOURCE, bad, "x").is_err());
    }
    let mut s = FeedState::new(SOURCE, "p");
    assert!(s.is_stale(ts("2026-09-13T00:00:00Z")), "never fetched");
    s.add_coverage(day_range("2026-09-15").unwrap());
    s.add_coverage(day_range("2026-09-13").unwrap());
    s.add_coverage(day_range("2026-09-14").unwrap());
    assert_eq!(s.coverage.len(), 1);
    assert!(s.covers(&TimeRange::new(
        ts("2026-09-13T05:00:00Z"),
        ts("2026-09-15T23:00:00Z")
    )));
    s.add_coverage(day_range("2026-09-20").unwrap());
    assert_eq!(s.coverage.len(), 2);

    // Repository time-window query: closed overlap, source filter, deterministic order.
    let mut repo = Repository::open_in_memory().unwrap();
    let adapter = GpsjamAdapter::default();
    let body =
        "hex,count_good_aircraft,count_bad_aircraft\n8419453ffffffff,30,2\n84194edffffffff,40,12\n";
    ingest_snapshot(
        &cache,
        &mut repo,
        &adapter,
        "2026-09-14",
        body,
        ts("2026-09-15T01:00:00Z"),
    )
    .unwrap();
    ingest_snapshot(
        &cache,
        &mut repo,
        &adapter,
        "2026-09-13",
        body,
        ts("2026-09-14T01:00:00Z"),
    )
    .unwrap();
    let window = TimeRange::new(ts("2026-09-13T23:59:59Z"), ts("2026-09-14T00:00:00Z"));
    let got: Vec<String> = repo
        .external_events_overlapping(&window, None)
        .unwrap()
        .into_iter()
        .map(|e| e.native_id)
        .collect();
    // Same start: by native id as text ('5' < 'e').
    assert_eq!(
        got,
        [
            "2026-09-13/8419453ffffffff",
            "2026-09-13/84194edffffffff",
            "2026-09-14/8419453ffffffff",
            "2026-09-14/84194edffffffff"
        ]
    );
    let edge = TimeRange::instant(day_range("2026-09-13").unwrap().end);
    assert_eq!(
        repo.external_events_overlapping(&edge, Some(SOURCE))
            .unwrap()
            .len(),
        2,
        "closed interval end"
    );
    assert!(
        repo.external_events_overlapping(&window, Some("kp"))
            .unwrap()
            .is_empty()
    );
    assert!(
        repo.external_events_overlapping(
            &TimeRange::new(ts("2026-09-14T00:00:00Z"), ts("2026-09-13T00:00:00Z")),
            None
        )
        .is_err()
    );
    fs::remove_dir_all(dir).ok();
}
