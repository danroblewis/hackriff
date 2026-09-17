//! T-406, the other half: an iterative scan **through the SDR device interface** really steps the
//! tune, and what each step saw lands in the **shared spectrum-history pyramid** — the one the big
//! view (T-408) and the survey bar (T-405) read — not in an accumulator of its own.
//!
//! The mock SDR device serves the recording behind the same interface the HackRF source implements,
//! and honours retunes (T-057), so "the scan stepped the radio" is observable here as *the analysed
//! data carried several different centres*, which is a fact about the device and not about the
//! scheduler's intentions.
//!
//! What it proves:
//!
//! 1. The scan **retunes the device** across the planned range — several distinct centres, one
//!    capture at a time, each recorded with its `device_id` (T-343/T-378).
//! 2. Each step wrote **its own** observation record, so the coverage read off the log spreads
//!    across the range rather than claiming one wide window.
//! 3. The **pyramid** holds cells across the swept range afterwards: the survey is retained where
//!    every other consumer already looks. Nothing in T-406 writes a second store.
//! 4. Spectrum the scan never reached stays **unobserved** in the coverage map — the accumulation
//!    does not paint the unreached band quiet.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use hk_core::scheduler::is_scan_step;
use hk_core::{MockEnd, Pacing};
use hk_model::attention::observation::ObservationRecord;
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay};
use hk_store::coverage::{Coverage, Device, grid_over};
use hk_store::history::{RegionQuery, Resolution};
use hk_store::observation::{
    MAX_RECORD_LIMIT, ObservationLogConfig, ObservationStore, RecordQuery,
};

/// Recording rate, and the scan's window rate: 1.5 MHz usable per step.
const FS: f64 = 2e6;
/// Centre of the recorded tone.
const CENTER_HZ: f64 = 433.92e6;
/// The band the scan walks.
const LO_HZ: f64 = 433.0e6;
const HI_HZ: f64 = 440.0e6;
/// A short dwell, so the test is a test. The policy is configurable on purpose; 10-30 s is what
/// the survey is *sized* for, and `crates/hk-core/src/scheduler/scan.rs` says why.
const DWELL_S: f64 = 0.25;

#[test]
fn an_iterative_scan_steps_the_mock_device_and_retains_the_survey_in_the_shared_pyramid() {
    let dir = TempDir::new("t406-device");
    let rec = tone_recording(&dir.0.join("src"), "tone", FS, 2.0, CENTER_HZ, None);
    let replay = open_mock_replay(&rec, Pacing::Unpaced, MockEnd::Loop).unwrap();
    let control = replay.source.mock_control();

    let scan = hk_core::scheduler::IterativeScan::from_seconds(DWELL_S).unwrap();
    assert!(
        !scan.recommended(),
        "a test dwell is deliberately outside the 10-30 s the survey is sized for"
    );
    let mut plan = scan.plan_over(
        "iterative scan",
        FreqRange::new(LO_HZ, HI_HZ),
        replay.info.start_time,
    );
    // The window rate is the recording's, so a step is 1.5 MHz of usable span and the range is
    // several steps wide. Everything else about the policy is the plan's own.
    plan.extra["scheduler"]["sweep_rate_hz"] = serde_json::json!(FS);

    let data_dir = dir.0.join("data");
    let mut cfg = PipelineConfig::new(&data_dir, plan).unwrap();
    cfg.source_class = replay.class;
    cfg.drive_scheduler = true;
    cfg.device_id = replay.device.device_id.clone();
    let t0 = replay.info.start_time;
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let product = handle.floor_product();

    // Four whole passes of stream time: every step in the range gets several turns, with slack —
    // a record is written when its step ends, so the step in flight at stop has none.
    let steps_per_pass = ((HI_HZ - LO_HZ) / (FS * 0.75)).ceil() as i64;
    let want_ns = 8 * steps_per_pass * (DWELL_S * 1e9) as i64;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let tick = |what: &str, f: &dyn Fn() -> bool| {
        while !f() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    // The capture thread publishes stream time only once it has read a block, so take the origin
    // after it starts rather than from the zero it holds before.
    tick("the capture thread starts", &|| {
        counters.stream_time_ns.load(Ordering::Relaxed) > 0
    });
    let s0 = counters.stream_time_ns.load(Ordering::Relaxed);
    tick("two passes of stream time", &|| {
        counters.stream_time_ns.load(Ordering::Relaxed) >= s0 + want_ns
    });
    handle.stop();
    let (summary, stopped) = wait_guarded(handle, Duration::from_secs(60));
    assert!(!stopped, "the run stopped when asked");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    // 1. The scan drove the DEVICE, not just the scheduler: the mock applied real control changes.
    let stats = control.mock_stats();
    assert!(
        summary.counter("/scheduler/steps") > 1,
        "the scheduler emitted steps"
    );
    assert!(
        stats.control_changes > 0,
        "the scan retuned the mock device: {stats:?}"
    );

    // 2. One record per step, with its own band — read back out of the log the run wrote.
    let store =
        ObservationStore::open(ObservationLogConfig::new(data_dir.join("observations"))).unwrap();
    let window = TimeRange::new(
        t0.saturating_add_nanos(-1_000_000_000),
        Timestamp::from_unix_nanos(counters.stream_time_ns.load(Ordering::Relaxed))
            .saturating_add_nanos(1_000_000_000),
    );
    let band = FreqRange::new(LO_HZ - 2.0 * FS, HI_HZ + 2.0 * FS);
    let page = store.query(&RecordQuery {
        freq: band,
        span: window,
        tier: None,
        cursor: 0,
        limit: MAX_RECORD_LIMIT,
    });
    let dwells: Vec<_> = page
        .records
        .iter()
        .filter_map(|r| match r {
            ObservationRecord::Dwell(d) if is_scan_step(d) => Some(d),
            _ => None,
        })
        .collect();
    // Measured, not guessed (the T-383 rule): runs of this test each wrote 12 scan records over
    // 10 distinct centres (5 hops × the two DC-dither parities) in 2.2 s wall, so a floor of one
    // pass has ~2.4x margin and the 120 s deadline has ~50x. The replay is UNPACED rather than
    // real-time: the assertions are about what the scan did, not about wall-clock pacing, and an
    // unpaced run neither competes with the suite's real-time tests nor is competed with.
    assert!(
        dwells.len() >= steps_per_pass as usize,
        "{} scan records for {steps_per_pass} steps per pass",
        dwells.len()
    );
    assert!(
        !page
            .records
            .iter()
            .any(|r| matches!(r, ObservationRecord::Sweep(_))),
        "an iterative scan writes no aggregated sweep record"
    );
    let mut centres: Vec<i64> = dwells
        .iter()
        .map(|d| (d.window.center_hz / 1e5).round() as i64)
        .collect();
    centres.sort_unstable();
    centres.dedup();
    assert!(
        centres.len() >= 3,
        "the tune stepped across the range: centres (×100 kHz) {centres:?}"
    );
    for d in &dwells {
        assert_eq!(
            d.device_id.as_deref(),
            Some(replay.device.device_id.as_str()),
            "every record names the front end that looked"
        );
    }

    // 3. Coverage built from those records spreads across the swept range — and stops there.
    let read = hk_store::spans_from_records(&page.records, &page.geometries, band);
    let grid = grid_over(
        &read.spans,
        &Device::Id(replay.device.device_id.clone()),
        band,
        window,
        1,
        512,
    );
    let covered: Vec<f64> = (0..grid.nf)
        .filter(|f| grid.cell(0, *f).is_some_and(Coverage::is_observed))
        .filter_map(|f| grid.freq_of(f).map(|r| 0.5 * (r.lo_hz + r.hi_hz)))
        .collect();
    let lo = covered.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = covered.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    assert!(
        hi - lo > 0.5 * (HI_HZ - LO_HZ),
        "the survey covers the range it walked: {:.3}-{:.3} MHz",
        lo / 1e6,
        hi / 1e6
    );
    // 4. And spectrum the scan never reached is unobserved, not quiet. One window's usable span
    //    beyond the plan's edge cannot have been sampled by any step.
    let beyond = HI_HZ + 1.5 * FS;
    assert_eq!(
        grid.at(beyond),
        Some(&Coverage::Unobserved),
        "spectrum the scan never reached must stay unobserved"
    );

    // 5. What each step saw is in the SHARED pyramid: the same store `/api/history`, the survey bar
    //    and the big view read. T-406 adds no accumulator of its own, and this is the assertion
    //    that keeps it that way.
    let p = product.lock().unwrap();
    let pyramid = p.uncalibrated_pyramid();
    let t_end = pyramid.latest_frame_end().expect("the pyramid took frames");
    let history = pyramid
        .query(&RegionQuery {
            freq: FreqRange::new(LO_HZ, HI_HZ),
            time: TimeRange::new(t0, t_end.saturating_add_nanos(1_000_000_000)),
            resolution: Resolution::MaxCells { t: 64, f: 256 },
        })
        .unwrap();
    let observed_f: Vec<usize> = (0..history.nf)
        .filter(|f| (0..history.nt).any(|t| history.cells[t * history.nf + f].frames > 0))
        .collect();
    assert!(
        observed_f.len() * 2 >= history.nf,
        "the pyramid holds the survey across the swept range: {} of {} frequency cells",
        observed_f.len(),
        history.nf
    );
    eprintln!(
        "[T-406] {} scan records, {} distinct centres, pyramid level {} with {}/{} frequency \
         cells holding frames over {:.3}-{:.3} MHz",
        dwells.len(),
        centres.len(),
        history.level,
        observed_f.len(),
        history.nf,
        LO_HZ / 1e6,
        HI_HZ / 1e6
    );
}
