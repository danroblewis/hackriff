//! **T-508 — a failed retune or re-plumb restarts capture; a run that has really ended says so.**
//!
//! T-497 closed one road into the user's report (*"the Retune button kills the live view; `hk
//! serve` stays up; it never recovers"* — three times): a settle wait with no bound. This file is
//! the other road. `supervise()` ended the run **for good** whenever a segment did not come back
//! cleanly, and `hk serve` then stayed up answering `finished: true` to a client that showed a
//! frozen edge as if it were live. Every way in is driven here through the generic device
//! interface (CLAUDE.md: e2e goes through the SDR device contract):
//!
//! 1. **The device refuses the retune when it applies it** — the mock SDR's
//!    [`MockFault::RetuneApplyFails`], which is how the HackRF driver fails: controls are applied
//!    on the capture thread, so a failed libhackrf call is a *read* error in the new segment, after
//!    the re-plumb has already answered OK. On `main` the capture thread's error ended the segment,
//!    nothing had requested a re-plumb, and the supervisor took that as the end of the run.
//!    Transient (one refusal) and persistent (every retune refused) are separate tests, because they
//!    recover to different windows.
//! 2. **A thread of the old segment still holds its state** past `unwrap_shared`'s 5 s bound — on
//!    `main`, "a thread of the previous segment still holds its state" and the run ended.
//! 3. **The new segment fails to start** — on `main` its parts were consumed and the run ended.
//! 4. **The front end is really gone** — every read fails. Recovery cannot help, and the run
//!    must end **visibly**: `capture: ended` with the cause, never a silent `finished`.
//!
//! **Non-vacuity, measured.** With recovery and salvage switched off — `recover` returning at once
//! and `take_parts` refusing to salvage, which is `main`'s behaviour (`main` itself cannot compile
//! these, having neither the `capture` field nor the seams) — all five fail: the refused-once case
//! with "0 samples in 60s" and `finished: true, capture: Ended` ("the front end stopped delivering:
//! … injected fault"); the refused-always case never gets capture back in 60 s; the salvage and
//! failed-start cases get `Err` from `retune` ("the re-plumb failed and capture could not be
//! restarted: … still holds its state"); the gone case ends without ever reporting `recovering`.
//! The browser tier's red on a real `main` build is recorded in `ui/e2e/canvas-journey.e2e.mjs`
//! (tests 5 and 6).

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::{TempDir, tone_recording};
use hk_core::{MockEnd, MockFault, MockOptions, MockSdrControl, MockSdrDriver, Pacing};
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::stats::Counters;
use hk_pipeline::{
    CaptureState, MAX_RECOVERY_ATTEMPTS, Pipeline, PipelineConfig, PipelineController,
    PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use serde_json::json;

const LIMIT: Duration = Duration::from_secs(60);

/// Samples that got past the window guard into the ring: "the live view has something to draw".
fn captured(c: &Counters) -> u64 {
    c.source.samples.load(Ordering::Relaxed)
}

fn stat(controller: &PipelineController, key: &str) -> u64 {
    controller.status().stats[key].as_u64().unwrap_or(0)
}

/// Waits until capture has delivered `n` more samples than `from`, panicking with `why` (and the
/// run's state) if it does not within [`LIMIT`].
fn wait_captured(handle: &PipelineHandle, from: u64, n: u64, why: &str) {
    let counters = handle.counters();
    let controller = handle.controller();
    let deadline = Instant::now() + LIMIT;
    while captured(&counters) < from + n {
        if Instant::now() > deadline {
            panic!(
                "{why}: {} samples in {LIMIT:?}; run state {:?}",
                captured(&counters) - from,
                controller.status()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn config(dir: &TempDir, center: f64, fs: f64, t0: Timestamp) -> PipelineConfig {
    let mut plan = replay_plan(center, fs, t0);
    plan.extra = json!({ "pipeline": { "ring_s": 0.5 } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(center, fs);
    // The configuration `hk serve --device …` runs: a live, window-classed run.
    cfg.live_window_class = true;
    cfg.settings.chains = Some(Vec::new());
    cfg
}

// ---------------------------------------------------------------------------------------------
// Through the mock SDR, with a fault: the HackRF's own way of failing a retune
// ---------------------------------------------------------------------------------------------

const MOCK_CENTER: f64 = 100.0e6;
const MOCK_FS: f64 = 1.0e6;
/// Inside the recording's band, at a new rate: a real move, and a re-plumb.
const MOCK_TO: (f64, f64) = (100.1e6, 2.0e6);

/// A real-time run of the mock SDR over a looping tone recording, with `fault` armed.
fn mock_run(dir: &TempDir, fault: MockFault) -> (PipelineHandle, Arc<MockSdrControl>) {
    let meta = tone_recording(&dir.0.join("rec"), "tone", MOCK_FS, 2.0, MOCK_CENTER, None);
    let driver = MockSdrDriver::new(
        &meta,
        MockOptions {
            end: MockEnd::Loop,
            block_len: 16_384,
            pacing: Pacing::RealTime { speed: 1.0 },
            fault: Some(fault),
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let ctl = source.mock_control();
    let info = SourceInfo {
        sample_rate_hz: MOCK_FS,
        center_hz: MOCK_CENTER,
        start_time: source.start_time(),
    };
    let cfg = config(dir, MOCK_CENTER, MOCK_FS, info.start_time);
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    (handle, ctl)
}

#[test]
fn a_retune_the_device_refuses_once_restarts_capture_on_the_requested_window() {
    let dir = TempDir::new("t508-refused-once");
    let (handle, ctl) = mock_run(&dir, MockFault::RetuneApplyFails { count: 1 });
    let controller = handle.controller();
    wait_captured(&handle, 0, 100_000, "the run never warmed up");

    // The re-plumb itself answers OK — the device only refuses when the new segment applies it,
    // which is exactly why this was invisible to the control plane on `main`.
    let out = controller
        .retune(MOCK_TO.0, MOCK_TO.1)
        .expect("the re-plumb answers before the device refuses");
    assert!(out.replumbed, "{out:?}");
    let at_retune = captured(&handle.counters());

    wait_captured(
        &handle,
        at_retune,
        400_000,
        "capture never came back after the device refused one retune — THIS IS T-508: hk serve \
         answers and the live capture is gone for ever",
    );
    let st = controller.status();
    assert_eq!(ctl.mock_stats().faults, 1, "the fault fired exactly once");
    assert!(!st.finished, "the run ended: {st:?}");
    assert_eq!(st.capture, CaptureState::Running, "{st:?}");
    assert_eq!(
        st.capture_note, None,
        "a running capture carries no failure note: {st:?}"
    );
    assert_eq!(
        (st.center_hz, st.sample_rate_hz),
        MOCK_TO,
        "a transient refusal is retried on the window the user asked for, and lands"
    );
    assert!(stat(&controller, "capture_failures") >= 1);
    assert!(stat(&controller, "capture_recoveries") >= 1);

    // A later, ordinary retune is not a recovery, and is never reported as one.
    let back = controller
        .retune(MOCK_CENTER, MOCK_FS)
        .expect("an ordinary retune");
    assert!(back.replumbed);
    let st = controller.status();
    assert_eq!(st.capture, CaptureState::Running, "{st:?}");
    assert_eq!(st.capture_note, None);

    handle.stop();
    handle.wait().unwrap();
    let st = controller.status();
    assert_eq!(st.capture, CaptureState::Ended);
    assert_eq!(
        st.capture_note, None,
        "a requested stop carries no note, whatever an earlier recovered failure said"
    );
}

#[test]
fn a_retune_the_device_always_refuses_falls_back_to_the_last_window_that_delivered() {
    let dir = TempDir::new("t508-refused-always");
    let (handle, ctl) = mock_run(&dir, MockFault::RetuneApplyFails { count: u32::MAX });
    let controller = handle.controller();
    wait_captured(&handle, 0, 100_000, "the run never warmed up");

    controller
        .retune(MOCK_TO.0, MOCK_TO.1)
        .expect("the re-plumb answers before the device refuses");
    let at_retune = captured(&handle.counters());

    // While it recovers, the control plane SAYS so — the state the user could never see.
    let mut saw_recovering = None;
    let deadline = Instant::now() + LIMIT;
    while captured(&handle.counters()) < at_retune + 400_000 {
        let st = controller.status();
        if st.capture == CaptureState::Recovering && saw_recovering.is_none() {
            saw_recovering = Some(st.capture_note.clone());
        }
        assert!(
            Instant::now() < deadline,
            "capture never came back after the device refused every retune: {st:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let note = saw_recovering
        .expect("capture came back without the control plane ever reporting it was recovering");
    assert!(
        note.as_deref()
            .is_some_and(|n| n.contains("injected fault")),
        "the recovering state names its cause: {note:?}"
    );

    let st = controller.status();
    assert!(!st.finished, "{st:?}");
    assert_eq!(st.capture, CaptureState::Running, "{st:?}");
    assert_eq!(
        (st.center_hz, st.sample_rate_hz),
        (MOCK_CENTER, MOCK_FS),
        "a device that refuses every retune is put back on the last window that delivered — and \
         the control plane reports that window, not the one it was refused"
    );
    assert!(
        ctl.mock_stats().faults >= 2,
        "retried once on the requested window first"
    );
    assert!(stat(&controller, "capture_recoveries") >= 2);

    handle.stop();
    handle.wait().unwrap();
}

// ---------------------------------------------------------------------------------------------
// Through the scripted radio: the re-plumb's own failures, and a front end that is gone
// ---------------------------------------------------------------------------------------------

const CENTER: f64 = 433.92e6;
const FS: f64 = 500e3;
const MOVE_HZ: f64 = 400e3;

fn radio_run(dir: &TempDir) -> (PipelineHandle, Arc<radio::RadioControl>) {
    let (rx, ctl) = radio::Radio::new(CENTER, FS, 16_384, radio::tone(|_| 80e3));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let handle = Pipeline::start(
        config(dir, CENTER, FS, t0),
        Box::new(rx),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    (handle, ctl)
}

fn finish_radio(handle: PipelineHandle, ctl: &radio::RadioControl) {
    handle.stop();
    ctl.finish();
    ctl.run_free();
    handle.wait().unwrap();
}

#[test]
fn a_segment_still_held_by_a_straggler_is_salvaged_not_the_end_of_the_run() {
    let dir = TempDir::new("t508-salvaged");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    wait_captured(&handle, 0, 200_000, "the run never warmed up");

    // A thread of the old segment that does not let go — what a chain or tap thread outliving its
    // segment does.
    let hold = handle.hold_segment().expect("a running segment");
    let out = controller.retune(CENTER + MOVE_HZ, FS * 2.0);
    let at_retune = captured(&handle.counters());
    assert!(
        matches!(out, Ok(o) if o.replumbed),
        "the re-plumb must complete on a salvaged state: {out:?} (on main: the run ended)"
    );
    wait_captured(
        &handle,
        at_retune,
        200_000,
        "capture never resumed after a salvaged re-plumb",
    );
    assert_eq!(stat(&controller, "segments_salvaged"), 1);
    let st = controller.status();
    assert!(!st.finished, "{st:?}");
    assert_eq!(st.capture, CaptureState::Running);
    assert_eq!(
        (st.center_hz, st.sample_rate_hz),
        (CENTER + MOVE_HZ, FS * 2.0)
    );
    drop(hold);
    finish_radio(handle, &ctl);
}

#[test]
fn a_segment_that_fails_to_start_is_started_again() {
    let dir = TempDir::new("t508-start-fails");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    wait_captured(&handle, 0, 200_000, "the run never warmed up");

    handle.fail_segment_starts(1);
    let out = controller.retune(CENTER + MOVE_HZ, FS * 2.0);
    assert!(
        matches!(out, Ok(o) if o.replumbed),
        "a failed start is retried, and the retune it served lands: {out:?}"
    );
    let at_retune = captured(&handle.counters());
    wait_captured(
        &handle,
        at_retune,
        200_000,
        "capture never resumed after a segment failed to start",
    );
    assert_eq!(stat(&controller, "replumb_failures"), 1);
    assert_eq!(stat(&controller, "capture_recoveries"), 1);
    let st = controller.status();
    assert!(!st.finished, "{st:?}");
    assert_eq!(st.capture, CaptureState::Running);
    assert_eq!(
        (st.center_hz, st.sample_rate_hz),
        (CENTER + MOVE_HZ, FS * 2.0)
    );
    finish_radio(handle, &ctl);
}

#[test]
fn a_front_end_that_is_gone_ends_the_run_and_the_control_plane_says_why() {
    let dir = TempDir::new("t508-gone");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    wait_captured(&handle, 0, 200_000, "the run never warmed up");

    ctl.fail_reads(true);
    let mut saw_recovering = false;
    let deadline = Instant::now() + LIMIT;
    let st = loop {
        let st = controller.status();
        saw_recovering |= st.capture == CaptureState::Recovering;
        if st.finished {
            break st;
        }
        assert!(
            Instant::now() < deadline,
            "the run neither recovered nor ended: {st:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        saw_recovering,
        "the run ended without ever trying to recover"
    );
    assert_eq!(st.capture, CaptureState::Ended, "{st:?}");
    let note = st.capture_note.clone().unwrap_or_default();
    assert!(
        note.contains("could not be restarted") && note.contains("the front end is gone"),
        "an ended run states why, with the device's own error: {note:?}"
    );
    assert_eq!(
        stat(&controller, "capture_recoveries"),
        u64::from(MAX_RECOVERY_ATTEMPTS),
        "every attempt started a segment, and every one of them died on its first read"
    );
    ctl.finish();
    ctl.run_free();
    handle.wait().unwrap();
}
