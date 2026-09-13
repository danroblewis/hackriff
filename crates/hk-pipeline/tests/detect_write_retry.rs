//! Data integrity of the detection reader's batched writes (T-027 review fix). An SQLite trigger
//! makes every provenance insert fail while a flag row exists, so the detection writer cannot
//! intern the stream's provenance and no detection can be stored. Track links name detections, so
//! they must wait; the failed detections must be kept and retried, not dropped. Once the flag is
//! cleared every detection is stored, every link reaches the database, and no link names a
//! missing detection.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use hk_core::Pacing;
use hk_model::Repository;
use serde_json::json;

#[test]
fn failed_detection_writes_are_retried_and_their_track_links_wait_for_them() {
    let src = TempDir::new("retry-src");
    let meta = tone_recording(&src.0, "tone", 250e3, 3.0, 433.5e6, None);
    let dir = TempDir::new("retry");
    let db = dir.0.join("hackriff.db");
    drop(Repository::open(&db).unwrap());
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    conn.execute_batch(
        "CREATE TABLE test_fail_provenance (x INTEGER);
         INSERT INTO test_fail_provenance VALUES (1);
         CREATE TRIGGER test_fail_provenance BEFORE INSERT ON provenance
         WHEN EXISTS (SELECT 1 FROM test_fail_provenance)
         BEGIN SELECT RAISE(ABORT, 'test: injected provenance write failure'); END;",
    )
    .unwrap();

    // Paced, so the failure window spans several flushes before the flag is cleared.
    let (cfg, replay) = replay_config(&dir.0, &meta, json!({}), Pacing::RealTime { speed: 1.0 });
    let handle = start(cfg, replay);
    let counters = handle.counters();
    let deadline = Instant::now() + Duration::from_secs(60);
    while counters.detect.db_errors.load(Ordering::Relaxed) < 2 {
        assert!(
            Instant::now() < deadline,
            "no injected write failure was hit"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        count("SELECT count(*) FROM detection"),
        0,
        "writes are failing"
    );
    assert_eq!(
        count("SELECT count(*) FROM track_detection"),
        0,
        "no track link is written before its detections"
    );
    conn.execute_batch("DELETE FROM test_fail_provenance;")
        .unwrap();

    let (s, fired) = wait_guarded(handle, Duration::from_secs(180));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert!(s.counter("/detect/db_errors") >= 2);
    let detections = s.counter("/detect/detections");
    assert!(detections > 0, "the tone is detected");
    assert_eq!(
        s.counter("/detect/detections_written"),
        detections,
        "every detection that failed to write was retried"
    );
    assert_eq!(s.detections_stored, detections);
    let links = count("SELECT count(*) FROM track_detection");
    assert!(links > 0, "track links were written after the retry");
    assert_eq!(links as u64, s.counter("/detect/track_links"));
    assert_eq!(
        count(
            "SELECT count(*) FROM track_detection td LEFT JOIN detection d \
             ON d.detection_id = td.detection_id WHERE d.detection_id IS NULL"
        ),
        0,
        "no link names a missing detection"
    );
    assert!(s.counter("/detect/track_rows") > 0);
}
