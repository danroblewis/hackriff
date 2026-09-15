//! T-115 observation log store tests: append/query/totals, hour segments named by sample time,
//! crash recovery, retention by sample-clock age and byte quota (fast-forward), and a stalled
//! writer that drops and counts without blocking producers.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hk_model::attention::baseline::SiteKey;
use hk_model::attention::observation::{
    DwellRecord, HopVisit, ObservationRecord, ObservedWindow, Reason, SweepGeometry, SweepRecord,
    Tier,
};
use hk_model::{FreqRange, TimeRange, Timestamp};

use super::segment::{HOUR_NS, hour_of, segment_path};
use super::*;

/// 2026-09-13T00:00:00Z.
const T0: i64 = 1_789_257_600_000_000_000;
const S: i64 = 1_000_000_000;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-observation-{tag}-{}-{:?}",
            std::process::id(),
            Instant::now()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn t(ns: i64) -> Timestamp {
    Timestamp::from_unix_nanos(ns)
}

fn window(center: f64) -> ObservedWindow {
    ObservedWindow {
        center_hz: center,
        sample_rate_hz: 2e6,
        usable: FreqRange::new(center - 1e6, center + 1e6),
        dc_excluded: Some(FreqRange::new(center - 15e3, center + 15e3)),
        rbw_hz: 1e3,
    }
}

fn geometry() -> SweepGeometry {
    SweepGeometry {
        schema: 1,
        id: 42,
        plan_version: 1,
        hops: vec![window(100e6), window(101.5e6)],
    }
}

/// A sweep record at `start`: hop 0 then hop 1, 50 ms each.
fn sweep(start: i64) -> ObservationRecord {
    ObservationRecord::Sweep(SweepRecord {
        schema: 1,
        survey_id: None,
        plan_version: 1,
        site: SiteKey::Unassigned,
        geometry: 42,
        span: TimeRange::new(t(start), t(start + 100_000_000)),
        visits: vec![
            HopVisit {
                hop: 0,
                start_ms: 0,
                observed_ms: 50,
            },
            HopVisit {
                hop: 1,
                start_ms: 50,
                observed_ms: 50,
            },
        ],
        preempted_hops: 0,
        dropped_samples: 0,
        overload_hops: 0,
    })
}

fn dwell(start: i64, len: i64, center: f64, reason: Reason) -> ObservationRecord {
    ObservationRecord::Dwell(DwellRecord {
        schema: 1,
        survey_id: None,
        seq: 1,
        plan_version: 1,
        site: SiteKey::Unassigned,
        reason,
        tier: reason.tier(),
        window: window(center),
        rf_path: 0,
        planned: TimeRange::new(t(start), t(start + len)),
        observed: TimeRange::new(t(start), t(start + len)),
        preempted: false,
        dropped_samples: 0,
        overload: false,
        provenance_ref: None,
    })
}

fn store(dir: &TempDir) -> ObservationStore {
    ObservationStore::open(ObservationLogConfig::new(dir.0.join("observations"))).unwrap()
}

#[test]
fn observation_records_query_by_box_and_totals_count_visits_gaps_and_tiers() {
    let dir = TempDir::new("query");
    let s = store(&dir);
    s.append(&ObservationRecord::Geometry(geometry()));
    // Passes at 0, 1, 2, 3 s; a POI dwell at 100.5 MHz from 1.2 s for 0.5 s.
    for k in 0..4 {
        s.append(&sweep(T0 + k * S));
        if k == 1 {
            s.append(&dwell(
                T0 + 1_200_000_000,
                500_000_000,
                100.5e6,
                Reason::PoiDwell { poi: 3 },
            ));
        }
    }
    let span = TimeRange::new(t(T0), t(T0 + 4 * S));
    let page = s.query(&RecordQuery {
        freq: FreqRange::new(99e6, 102e6),
        span,
        tier: None,
        cursor: 0,
        limit: 3,
    });
    assert_eq!(page.records.len(), 3);
    assert_eq!(page.next_cursor, Some(3));
    assert_eq!(page.geometries, vec![geometry()]);
    let rest = s.query(&RecordQuery {
        freq: FreqRange::new(99e6, 102e6),
        span,
        tier: None,
        cursor: 3,
        limit: 3,
    });
    assert_eq!(rest.records.len(), 2);
    assert_eq!(rest.next_cursor, None);
    let bandit = s.query(&RecordQuery {
        freq: FreqRange::new(99e6, 102e6),
        span,
        tier: Some(Tier::Bandit),
        cursor: 0,
        limit: 10,
    });
    assert_eq!(bandit.records.len(), 1);

    // 99.5 MHz: only hop 0 covers it: 4 visits of 50 ms, 1 s apart.
    // 100.6 MHz (12.5 kHz wide): hops 0 and 1 back to back merge into one visit per pass; the
    // dwell adds a fifth visit (bandit tier, not activity independent).
    // 100.0 MHz: inside hop 0's DC notch and below hop 1: only the dwell (99.5–101.5 MHz)
    // observes it.
    // 100.99–101.01 MHz straddles hop 0's upper edge but hop 1 covers it whole (and so does the
    // dwell).
    let ch = |c: f64| FreqRange::centered(c, 12.5e3);
    let tot = s.totals(&[ch(99.5e6), ch(100.6e6), ch(100.0e6), ch(101e6)], span);
    for x in &tot {
        x.validate().unwrap();
    }
    assert_eq!(tot[0].n_visits, 4);
    assert_eq!(tot[0].n_visits_activity_independent, 4);
    assert!((tot[0].observed_s.background_sweep - 0.2).abs() < 1e-9);
    assert_eq!(tot[0].mean_revisit_s, Some(1.0));
    assert!(
        (tot[0].max_gap_s - 0.95).abs() < 1e-9,
        "{}",
        tot[0].max_gap_s
    );
    assert_eq!(tot[1].n_visits, 5);
    assert_eq!(tot[1].n_visits_activity_independent, 4);
    assert!((tot[1].observed_s.bandit - 0.5).abs() < 1e-9);
    assert_eq!(tot[2].n_visits, 1);
    assert_eq!(tot[2].n_visits_activity_independent, 0);
    assert!(
        (tot[2].max_gap_s - 2.3).abs() < 1e-9,
        "{}",
        tot[2].max_gap_s
    );
    assert_eq!(tot[2].mean_revisit_s, None);
    assert_eq!(tot[3].n_visits, 5);
    assert_eq!(tot[3].n_visits_activity_independent, 4);
    let gaps = coverage_gaps(span, &s.observations_of(ch(99.5e6), span), 900_000_000);
    assert_eq!(gaps.len(), 4, "{gaps:?}");
}

#[test]
fn observation_segments_are_hourly_by_sample_time_self_contained_and_recover_torn_tails() {
    let dir = TempDir::new("segments");
    let root = dir.0.join("observations");
    {
        let s = store(&dir);
        s.append(&ObservationRecord::Geometry(geometry()));
        s.append(&sweep(T0 + 10 * S));
        s.append(&sweep(T0 + HOUR_NS + 10 * S));
        // Buffered, not yet on disk for the open hour; queries still see it.
        let span = TimeRange::new(t(T0), t(T0 + 2 * HOUR_NS));
        assert_eq!(
            s.totals(&[FreqRange::centered(99.5e6, 1e3)], span)[0].n_visits,
            2
        );
        assert_eq!(s.stats().sealed.load(Ordering::Relaxed), 1, "hour 0 sealed");
    }
    let h0 = segment_path(&root, hour_of(t(T0)));
    let h1 = segment_path(&root, hour_of(t(T0 + HOUR_NS)));
    assert!(h0.ends_with("2026/09/13/00.log") && h1.ends_with("2026/09/13/01.log"));
    for p in [&h0, &h1] {
        let text = std::fs::read_to_string(p).unwrap();
        assert!(
            text.lines()
                .next()
                .unwrap()
                .contains("\"record\":\"geometry\""),
            "every segment starts with its geometry"
        );
    }
    // Tear the tail of hour 1 (a crash mid-write).
    let mut bytes = std::fs::read(&h1).unwrap();
    bytes.extend_from_slice(b"0badc0de {\"record\":\"sweep\",");
    std::fs::write(&h1, &bytes).unwrap();
    let s = store(&dir);
    assert!(
        std::fs::read(&h1).unwrap().ends_with(b"\n"),
        "torn tail truncated"
    );
    s.append(&sweep(T0 + HOUR_NS + 20 * S));
    s.flush();
    let span = TimeRange::new(t(T0 + HOUR_NS), t(T0 + 2 * HOUR_NS));
    let n = s.totals(&[FreqRange::centered(99.5e6, 1e3)], span)[0].n_visits;
    assert_eq!(
        n, 2,
        "only the hour-1 geometry is on hand, and it resolves both records"
    );
}

#[test]
fn observation_retention_deletes_whole_hours_by_sample_age_and_byte_quota() {
    let dir = TempDir::new("retention");
    let mut cfg = ObservationLogConfig::new(dir.0.join("observations"));
    cfg.max_age_ns = 24 * HOUR_NS;
    let s = ObservationStore::open(cfg).unwrap();
    s.append(&ObservationRecord::Geometry(geometry()));
    // Fast-forward: one pass per hour for 3 days of sample time (no wall time passes).
    for h in 0..72 {
        s.append(&sweep(T0 + h * HOUR_NS + 5 * S));
    }
    let hours = s.hours();
    let newest = hour_of(t(T0 + 71 * HOUR_NS));
    assert_eq!(*hours.last().unwrap(), newest);
    // Hours ending more than 24 h before the newest record are gone.
    assert_eq!(hours.first().copied(), Some(newest - 24), "{hours:?}");
    assert!(s.stats().segments_deleted.load(Ordering::Relaxed) >= 47);
    assert!(!segment_path(&dir.0.join("observations"), hour_of(t(T0))).exists());

    // Byte quota: shrink it to about three segments' worth; oldest hours go first.
    let seg = std::fs::metadata(segment_path(&dir.0.join("observations"), newest - 1))
        .unwrap()
        .len();
    drop(s);
    let mut cfg = ObservationLogConfig::new(dir.0.join("observations"));
    cfg.max_age_ns = 24 * HOUR_NS;
    cfg.max_bytes = 3 * seg + seg / 2;
    let s = ObservationStore::open(cfg).unwrap();
    s.append(&ObservationRecord::Geometry(geometry()));
    s.append(&sweep(T0 + 72 * HOUR_NS + 5 * S));
    assert!(s.bytes() <= 3 * seg + seg / 2, "{} > quota", s.bytes());
    let hours = s.hours();
    assert_eq!(*hours.last().unwrap(), newest + 1);
    assert!(
        hours.len() <= 4 && hours.windows(2).all(|w| w[1] == w[0] + 1),
        "{hours:?}"
    );
}

#[test]
fn observation_stalled_writer_drops_and_counts_without_blocking_producers() {
    let dir = TempDir::new("stall");
    let mut cfg = ObservationLogConfig::new(dir.0.join("observations"));
    cfg.queue_len = 8;
    let s = ObservationStore::open(cfg).unwrap();
    let writer = ObservationWriter::spawn(s.clone(), None).unwrap();
    let q = writer.queue();
    let guard = s.stall();
    let started = Instant::now();
    let mut accepted = 0u64;
    for k in 0..10_000 {
        if q.offer(sweep(T0 + k * S)) {
            accepted += 1;
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "offer blocked on a stalled writer"
    );
    let st = q.stats();
    assert_eq!(st.offered.load(Ordering::Relaxed), 10_000);
    assert!(
        accepted <= 9,
        "the queue holds 8 (+1 in the writer's hands)"
    );
    assert_eq!(st.dropped.load(Ordering::Relaxed), 10_000 - accepted);
    drop(guard);
    writer.finish();
    assert_eq!(s.stats().written.load(Ordering::Relaxed), accepted);
}
