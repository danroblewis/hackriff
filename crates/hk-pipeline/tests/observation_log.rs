//! T-115 end to end through the mock SDR: a scheduler-driven ScanPlan run over the mock device
//! yields an observation log whose coverage is exactly the tuned windows (every sweep hop's
//! analysed extent and DC notch, every dwell's window), whose sweep visits follow the schedule's
//! pass order, whose record counts match the scheduler's steps, and whose revisit counts match
//! the hop visits. Nothing is dropped on a healthy writer.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use hk_core::scheduler::observe::WindowRule;
use hk_core::scheduler::{HopKind, Scheduler, SchedulerConfig, SyntheticClock};
use hk_core::{MockEnd, Pacing, SourceControl};
use hk_model::attention::observation::{ObservationRecord, Tier};
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_pipeline::observe::{DC_NOTCH_HALF_HZ, HANN_ENBW_BINS};
use hk_pipeline::{
    Pipeline, PipelineConfig, TrackInventory, detection_resolution, open_mock_replay, replay_plan,
};
use hk_store::observation::RecordQuery;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "hk-t115-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
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

#[test]
fn observation_log_through_the_mock_sdr_matches_the_tuned_windows_and_the_schedule() {
    const FS: f64 = 2e6;
    const CENTER: f64 = 433.92e6;
    let dir = TempDir::new("e2e");
    let rec = tone_recording(&dir.0.join("src"), "tone", FS, 6.0, CENTER, None);
    let replay = open_mock_replay(&rec, Pacing::Unpaced, MockEnd::Stop).unwrap();
    let control = replay.source.mock_control();
    let caps = control.capabilities().clone();
    let plan = replay_plan(
        replay.info.center_hz,
        replay.info.sample_rate_hz,
        replay.info.start_time,
    );
    let mut cfg = PipelineConfig::new(dir.0.join("data"), plan.clone()).unwrap();
    cfg.source_class = replay.class;
    cfg.lossless = true;
    cfg.drive_scheduler = true;
    cfg.device_id = replay.device.device_id.clone();
    let (fft_len, _) = detection_resolution(FS, &cfg.settings);

    // The schedule, compiled independently exactly as the pipeline's scheduler compiles it.
    let mut scfg = SchedulerConfig::from_plan(&plan).unwrap();
    scfg.sweep_rate_hz = FS;
    scfg.dwell_min_rate_hz = FS;
    scfg.max_span_hz = FS;
    let expected = Scheduler::new(
        &plan,
        scfg,
        &caps,
        SyntheticClock::new(replay.info.start_time),
    )
    .unwrap()
    .plan()
    .clone();
    let n_hops = expected.hops.len();
    assert!(n_hops >= 2, "the plan needs more than one window");
    assert!(expected.hops.iter().all(|h| h.kind == HopKind::Sweep));

    let t0 = replay.info.start_time;
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let store = handle.observation_store().expect("the run opened its log");
    let (summary, stopped) = wait_guarded(handle, Duration::from_secs(120));
    assert!(!stopped, "the run finished on its own");
    assert!(
        control.mock_stats().control_changes > 0,
        "the scheduler retuned the mock"
    );

    let span = TimeRange::new(
        t0.saturating_add_nanos(-3_600_000_000_000),
        t0.saturating_add_nanos(3_600_000_000_000),
    );
    let page = store.query(&RecordQuery {
        freq: FreqRange::new(0.0, 7e9),
        span,
        tier: None,
        cursor: 0,
        limit: 10_000,
    });
    assert!(page.next_cursor.is_none());
    let rule = WindowRule {
        fft_bins: fft_len,
        dc_half_hz: DC_NOTCH_HALF_HZ,
        enbw_bins: HANN_ENBW_BINS,
    };

    // Coverage is exactly the tuned windows: one geometry, hop for hop.
    let [g] = page.geometries.as_slice() else {
        panic!("one geometry: {:?}", page.geometries);
    };
    assert_eq!(g.hops.len(), n_hops);
    for (hop, w) in expected.hops.iter().zip(&g.hops) {
        assert_eq!(*w, rule.window(hop.center_hz, hop.rate_hz));
        // The window around the tuned centre, less the DC notch.
        let dc = w.dc_excluded.unwrap();
        assert_eq!(dc.center_hz(), hop.center_hz);
        assert!(w.usable.lo_hz < hop.covers.lo_hz && hop.covers.hi_hz < w.usable.hi_hz);
    }

    let mut sweep_visits = Vec::new();
    let mut dwells = 0u64;
    for r in &page.records {
        r.validate().unwrap();
        match r {
            ObservationRecord::Sweep(s) => {
                assert_eq!(s.geometry, g.id);
                sweep_visits.extend(s.visits.iter().map(|v| (s.span.start, *v)));
            }
            ObservationRecord::Dwell(d) => {
                dwells += 1;
                assert_ne!(d.tier, Tier::BackgroundSweep);
                assert_eq!(d.window, rule.window(d.window.center_hz, FS));
            }
            ObservationRecord::Geometry(_) => unreachable!("geometries are returned apart"),
        }
    }
    // Every emitted step is one record: sweep hops as visits, the rest as dwells.
    let steps = |p: &str| summary.counter(&format!("/scheduler/{p}"));
    assert_eq!(steps("apply_errors"), 0);
    assert_eq!(sweep_visits.len() as u64, steps("sweep_steps"));
    assert_eq!(dwells, steps("dwell_steps") + steps("trust_steps"));
    assert!(
        sweep_visits.len() >= 2 * n_hops,
        "{} visits",
        sweep_visits.len()
    );
    // Visits follow the schedule's pass order.
    for w in sweep_visits.windows(2) {
        assert_eq!(w[1].1.hop as usize, (w[0].1.hop as usize + 1) % n_hops);
    }
    let settled = sweep_visits.iter().filter(|v| v.1.observed_ms > 0).count();
    assert!(
        settled * 10 >= sweep_visits.len() * 9,
        "{settled} of {} hops settled",
        sweep_visits.len()
    );

    // Revisit counts: a channel only hop 0 covers is visited once per hop-0 visit that settled.
    let w0 = &g.hops[0];
    let edge = if expected.hops[1].center_hz > expected.hops[0].center_hz {
        FreqRange::new(w0.usable.lo_hz + 1e3, w0.usable.lo_hz + 13.5e3)
    } else {
        FreqRange::new(w0.usable.hi_hz - 13.5e3, w0.usable.hi_hz - 1e3)
    };
    let inside = |w: &hk_model::attention::observation::ObservedWindow| {
        w.covered()
            .iter()
            .any(|c| c.lo_hz <= edge.lo_hz && edge.hi_hz <= c.hi_hz)
    };
    assert!(inside(w0) && g.hops[1..].iter().all(|w| !inside(w)));
    let hop0: Vec<Timestamp> = sweep_visits
        .iter()
        .filter(|v| v.1.hop == 0 && v.1.observed_ms > 0)
        .map(|v| {
            v.0.saturating_add_nanos(i64::from(v.1.start_ms) * 1_000_000)
        })
        .collect();
    let tot = &store.totals(&[edge], span)[0];
    tot.validate().unwrap();
    let dwell_hits = page
        .records
        .iter()
        .filter(|r| matches!(r, ObservationRecord::Dwell(d) if inside(&d.window) && d.observed.duration_ns() > 0))
        .count() as u64;
    // A visit is a maximal run of contiguous observed time: a dwell that starts as a hop-0 visit
    // ends is the same look. Merge the schedule's observations of the channel independently.
    let mut looks: Vec<(i64, i64, bool)> = sweep_visits
        .iter()
        .filter(|v| v.1.hop == 0 && v.1.observed_ms > 0)
        .map(|v| {
            let s = v.0.as_unix_nanos() + i64::from(v.1.start_ms) * 1_000_000;
            (s, s + i64::from(v.1.observed_ms) * 1_000_000, true)
        })
        .collect();
    looks.extend(page.records.iter().filter_map(|r| match r {
        ObservationRecord::Dwell(d) if inside(&d.window) && d.observed.duration_ns() > 0 => Some((
            d.observed.start.as_unix_nanos(),
            d.observed.end.as_unix_nanos(),
            false,
        )),
        _ => None,
    }));
    looks.sort_by_key(|l| l.0);
    let mut merged: Vec<(i64, i64, bool)> = Vec::new();
    for l in looks {
        match merged.last_mut() {
            Some(m) if l.0 <= m.1 => {
                m.1 = m.1.max(l.1);
                m.2 |= l.2;
            }
            _ => merged.push(l),
        }
    }
    assert!(merged.len() as u64 <= hop0.len() as u64 + dwell_hits);
    assert_eq!(tot.n_visits, merged.len() as u64);
    assert_eq!(
        tot.n_visits_activity_independent,
        merged.iter().filter(|m| m.2).count() as u64
    );
    let pass_s = (hop0[hop0.len() - 1].as_unix_nanos() - hop0[0].as_unix_nanos()) as f64 * 1e-9
        / (hop0.len() - 1) as f64;
    if dwell_hits == 0 {
        assert!((tot.mean_revisit_s.unwrap() - pass_s).abs() < 1e-3);
    }

    // A healthy writer drops nothing, and the log saw every offered record.
    assert_eq!(summary.counter("/observations/records_dropped"), 0);
    assert!(summary.counter("/observations/records_offered") > 0);
    let st = store.stats().to_json();
    assert_eq!(st["dropped"], 0);
    assert_eq!(
        st["written"].as_u64().unwrap(),
        summary.counter("/observations/records_offered")
    );
}
