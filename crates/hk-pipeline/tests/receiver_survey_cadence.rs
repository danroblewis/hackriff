//! T-399: the receiver-line survey has a production caller, and it runs **once per capture state**.
//!
//! T-394 measured the receiver's own cyclic lines instead of listing notches, and said plainly it
//! was not wired in: the survey wants a second or more of the raw tuned span and
//! `hk_pipeline::classify::classify_box` is handed one burst. The caller is the `hk-survey` ring
//! reader, and the property that matters about it is a **cadence**, not a number:
//!
//! 1. It runs at all, through the mock SDR device, from a cold start — the survey is measured on
//!    live capture, not by a test calling the estimator by hand.
//! 2. Its cost is charged **per capture state**, not per classification: a run that stays tuned
//!    measures once and then never again however long it runs, and a **retune** — a different
//!    receiver — starts a new one rather than reusing the old.
//! 3. A survey is never applied across a capture state. What is in force after the retune
//!    describes the new tune, and the old one is gone.
//!
//! It runs through the device interface (a mock SDR replaying a recording, retuned by the control
//! plane), which is where the standing rule puts every end-to-end assertion.

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use hk_core::{MockEnd, Pacing};
use hk_pipeline::{Pipeline, PipelineConfig, TrackInventory, open_mock_replay, replay_plan};

fn wait_for(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn t399_the_survey_runs_once_per_capture_state_and_never_once_per_classification() {
    const FS: f64 = 1e6;
    const CENTERS: [f64; 2] = [433.92e6, 436.42e6];
    let dir = TempDir::new("receiver-survey-cadence");
    // Long enough that a survey window fits inside one pass of the recording: the reader will not
    // splice a window across the loop point, which is a genuine discontinuity in the record.
    let rec = tone_recording(&dir.0.join("src"), "tone", FS, 8.0, CENTERS[0], None);
    let replay = open_mock_replay(&rec, Pacing::RealTime { speed: 8.0 }, MockEnd::Loop).unwrap();
    let plan = replay_plan(
        replay.info.center_hz,
        replay.info.sample_rate_hz,
        replay.info.start_time,
    );
    let mut cfg = PipelineConfig::new(dir.0.join("data"), plan).unwrap();
    cfg.source_class = replay.class;
    cfg.live_window_class = true;
    cfg.device_id = replay.device.device_id.clone();
    let handle = Pipeline::start(
        cfg,
        Box::new(replay.source),
        replay.info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let survey = handle.receiver_survey();
    let counters = handle.counters();
    let plane = handle.controller();
    let window_s = survey.config().window_s;
    let limit = Duration::from_secs(120);

    // --- The first capture state.
    wait_for("the first survey", limit, || survey.counts().attempts() > 0);
    let first = survey.counts();
    assert_eq!(first.states, 1, "one capture state so far: {first:?}");
    assert!(
        first.measured >= 1,
        "the survey ran on live capture from the mock device: {first:?}"
    );
    let lines = survey.current().expect("a survey in force");
    assert_eq!(
        lines.sample_rate_hz, FS,
        "the survey describes the tuned span it was measured over"
    );
    assert!(
        (lines.center_hz - CENTERS[0]).abs() < 1.0,
        "measured at the first centre: {lines:?}"
    );
    assert!(
        lines.source_samples >= (window_s * FS) as u64,
        "a capture window, not a burst: {} samples of {FS} Hz",
        lines.source_samples
    );
    eprintln!(
        "[T-399] state 1: {} lines over {} reference channels of {}, cell {:.2} Hz, \
         {:.0} ms to measure",
        lines.lines.len(),
        lines.reference_channels,
        lines.surveyed_channels,
        lines.resolution_hz,
        first.cost_us as f64 / 1e3,
    );

    // --- The cadence. Run on for several more survey windows of stream time: the measurement does
    // not repeat, however much capture goes by and however many boxes are classified under it.
    let s0 = counters.stream_time_ns.load(Ordering::Relaxed);
    let on = (4.0 * window_s * 1e9) as i64;
    wait_for("four more survey windows of stream time", limit, || {
        counters.stream_time_ns.load(Ordering::Relaxed) >= s0 + on
    });
    assert_eq!(
        survey.counts().attempts(),
        first.attempts(),
        "a settled capture state is surveyed once and then left alone"
    );

    // --- A retune is a different receiver.
    plane.retune(CENTERS[1], FS).expect("retune");
    wait_for("the tune reaches the capture", limit, || {
        (f64::from_bits(counters.tune_center_bits.load(Ordering::Relaxed)) - CENTERS[1]).abs() < 1.0
    });
    wait_for("the survey of the new state", limit, || {
        survey.counts().states >= 2
    });
    wait_for("a measurement at the new tune", limit, || {
        survey
            .current()
            .is_some_and(|r| (r.center_hz - CENTERS[1]).abs() < 1.0)
    });
    let second = survey.counts();
    assert_eq!(second.states, 2, "two capture states: {second:?}");
    let max = survey.counts().states * 3;
    assert!(
        second.attempts() <= max,
        "bounded by capture states, not by classifications: {second:?} over {max}"
    );
    eprintln!("[T-399] after the retune: {second:?}");

    handle.stop();
    let (summary, stopped) = wait_guarded(handle, Duration::from_secs(60));
    assert!(!stopped, "the run stopped when asked");
    // The survey reader is always-on and must never be what ends a run.
    assert!(
        summary.errors.is_empty(),
        "the survey reader ran clean: {:?}",
        summary.errors
    );
}
