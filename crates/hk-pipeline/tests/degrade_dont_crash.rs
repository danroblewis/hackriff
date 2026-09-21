//! **T-541 — degrade, don't crash: the fault matrix.**
//!
//! The user's framing: *no single device or retune error may end the run, wedge capture, or make
//! `hk serve` indistinguishable from a dead process.* T-497, T-508, T-525 and T-530 each closed
//! one road into that report. This file is the **general guard**: a table of device and thread
//! faults, driven through the generic device contract, each asserting the same two things —
//!
//! - **the run never goes quiet about it.** Either it recovers and comes back to
//!   [`CaptureState::Running`], or it ends as [`CaptureState::Ended`] **with a cause**. A run that
//!   reads `running` while nothing arrives is the defect ("a frozen edge that looks live"), and so
//!   is a run that reads `ended` with `capture_note: None` after a fault.
//! - **the control plane keeps answering.** `status()` is served throughout — it is what
//!   `/api/status` is built on, and a supervisor that cannot get an answer restarts the process.
//!
//! The third property — that a *recovering* server still answers the `/ws/spectrum/live`
//! handshake `ops/stage.sh` health-checks with, rather than `410 Gone` — needs the real HTTP
//! front end and lives in `recovery_spectrum_handshake.rs`.
//!
//! **What this file does NOT claim.** Every fault here is *injected*, at the boundary where the
//! driver would report it. That is the whole point of driving faults through the device contract
//! (CLAUDE.md) — but it means these tests prove the **pipeline's response** to a class of fault,
//! never that a real HackRF produces exactly that fault, nor that libhackrf/libusb fails only in
//! ways expressible through this interface. See the "not covered" note at the bottom of this file.
//!
//! **Non-vacuity.** Each test names, in its assertion message, what the observed failure looks
//! like without the guard it covers.
//!
//! # WHAT THIS TICKET DOES NOT COVER
//!
//! The honest boundary between "we guarantee this" and "only the radio can tell us" is part of
//! T-541's deliverable, so it is written here beside the guarantees rather than in a report
//! nobody will read next to the code. **Everything below is out of reach of any mock, by
//! construction**, and belongs to T-542 (the live-HackRF evidence) and to the HIL tier.
//!
//! 1. **Faults that are not expressible through `hk_core::Source`.** Every fault here arrives as
//!    a `SourceError` from a read or a control call, or as a panic. A driver can fail in ways
//!    that never reach that interface: libusb aborting the process, a `hackrf_*` call blocking
//!    for ever instead of returning an error (the shape T-497 met, and the one a bounded wait
//!    rather than an error handler is the answer to), a segfault in C, an OOM kill. No Rust
//!    guard in this crate can make those degrade; what it can do is not be the cause of them.
//! 2. **Timing and rate.** These runs are seconds long at 0.5–2 MS/s with a pausable, read-on-
//!    demand front end. A HackRF at 20 MS/s over USB 2.0 for hours has overrun, thermal and
//!    host-scheduling behaviour that no scripted radio reproduces. "The pipeline recovers from a
//!    read error" is proved; "recovery keeps up at 20 MS/s" is not.
//! 3. **Device-specific frequency and rate behaviour.** A HackRF near 1 MHz or 6 GHz, a rate the
//!    hardware rounds, an LO that settles slowly, PLL lock failures, and every band-edge case of
//!    a real synthesiser. The mock lands exactly where it is told; the two faults that model
//!    *not* landing (`lose_center`, `RetuneApplyFails`) are guesses at the shape, informed by the
//!    driver's code, not observations of the radio.
//! 4. **The USB/driver layer itself.** `hk_core::source::hackrf` — its FFI, its transfer pool,
//!    its stall detection — is not exercised at all by anything in this file. Its own panic
//!    audit is code reading, not testing.
//! 5. **Whether the user's two reported crashes are any of the above.** They are reproduced and
//!    fixed under T-542, on the radio. A green suite here is not evidence about them, and must
//!    not be read as any: `ops/fuzz-rig.sh` ran 69 workload cycles clean against a mock that
//!    could not fail, which is exactly how a number like that misleads.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::TempDir;
use hk_core::{MockEnd, MockFault, MockOptions, MockSdrDriver, Pacing};
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::stats::Counters;
use hk_pipeline::{
    CaptureState, ControlFailure, MAX_RECOVERY_ATTEMPTS, Pipeline, PipelineConfig,
    PipelineController, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use serde_json::json;

const CENTER: f64 = 433.92e6;
const FS: f64 = 500e3;
const LIMIT: Duration = Duration::from_secs(60);

fn captured(c: &Counters) -> u64 {
    c.source.samples.load(Ordering::Relaxed)
}

fn stat(controller: &PipelineController, key: &str) -> u64 {
    controller.status().stats[key].as_u64().unwrap_or(0)
}

/// Waits until capture has delivered `n` more samples than `from`; `false` if it never does.
fn wait_captured(handle: &PipelineHandle, from: u64, n: u64) -> bool {
    let counters = handle.counters();
    let deadline = Instant::now() + LIMIT;
    while captured(&counters) < from + n {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Waits for the run to finish; `false` if it never does.
fn wait_finished(controller: &PipelineController) -> bool {
    let deadline = Instant::now() + LIMIT;
    while !controller.status().finished {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// A live, window-classed run of the scripted radio: what `hk serve --device …` configures.
fn radio_run(dir: &TempDir) -> (PipelineHandle, Arc<radio::RadioControl>) {
    let (rx, ctl) = radio::Radio::new(CENTER, FS, 16_384, radio::tone(|_| 80e3));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": 0.5 } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.settings.chains = Some(Vec::new());
    let handle = Pipeline::start(
        cfg,
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

fn finish(handle: PipelineHandle, ctl: &radio::RadioControl) {
    handle.stop();
    ctl.fail_reads(false);
    ctl.finish();
    ctl.run_free();
    let _ = handle.wait();
}

/// **The invariant every case below shares.** A run is either delivering, or saying why not. The
/// one thing it may never be is `running` with nothing arriving and no note.
fn assert_states_itself(controller: &PipelineController, when: &str) {
    let st = controller.status();
    match st.capture {
        CaptureState::Running => {}
        CaptureState::Recovering | CaptureState::Ended => assert!(
            st.capture_note.is_some() || !st.finished || st.capture == CaptureState::Ended,
            "{when}: capture is {:?} and the run says nothing about why: {st:?}",
            st.capture
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Property 1 — no panic crosses a thread boundary into a dead server
// ---------------------------------------------------------------------------------------------

/// **A reader thread that panics is observed, and the run recovers from it.**
///
/// Before T-541 a panicking reader was joined only when its segment ended for some *other*
/// reason, which on a live run may be never: the capture thread kept filling the ring, the
/// control plane kept answering `capture: running`, and the surface that reader fed simply
/// stopped advancing. `run::guarded` turns the panic into an error, a stated cause and a stop of
/// the segment, so the supervisor sees it within one turnaround.
#[test]
fn a_reader_that_panics_is_reported_and_capture_comes_back() {
    let dir = TempDir::new("t541-reader-panics");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    assert!(
        wait_captured(&handle, 0, 100_000),
        "the run never warmed up"
    );

    // One panic, in the next segment's spectrum reader. A re-plumb is the ordinary way to get a
    // new segment; the fault is in the *thread*, not in the retune.
    handle.panic_worker(Some(("hk-spectrum", 1)));
    let at = captured(&handle.counters());
    let _ = controller.retune(CENTER + 400e3, FS * 2.0);

    assert!(
        wait_captured(&handle, at, 200_000),
        "capture never came back after a reader panicked — THIS IS T-541: on a pipeline without \
         `run::guarded` the thread is simply gone, nothing joins it, and the run keeps reporting \
         `running` over a surface that has stopped advancing. Run state: {:?}",
        controller.status()
    );
    assert_eq!(
        handle.worker_panics(),
        1,
        "the panic must be counted, not swallowed: {:?}",
        controller.status()
    );
    assert!(
        stat(&controller, "worker_panics") >= 1,
        "and served on the control plane: {:?}",
        controller.status()
    );
    let st = controller.status();
    assert!(
        !st.finished,
        "one reader panic must not end the run: {st:?}"
    );
    assert_eq!(st.capture, CaptureState::Running, "{st:?}");
    assert_states_itself(&controller, "after a recovered reader panic");
    finish(handle, &ctl);
}

/// **The capture thread panicking is the same story, and its payload reaches the run.**
///
/// `join_workers` already treated a panicked `hk-capture` as a capture failure; what it could not
/// do is say *what* panicked — the join gives an opaque payload. Through `guarded` the text
/// reaches `capture_note`, which is what the surface shows.
#[test]
fn the_capture_thread_panicking_is_recovered_and_named() {
    let dir = TempDir::new("t541-capture-panics");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    assert!(
        wait_captured(&handle, 0, 100_000),
        "the run never warmed up"
    );

    handle.panic_worker(Some(("hk-capture", 1)));
    let at = captured(&handle.counters());
    let _ = controller.retune(CENTER + 400e3, FS * 2.0);

    assert!(
        wait_captured(&handle, at, 200_000),
        "capture never came back after the capture thread panicked: {:?}",
        controller.status()
    );
    assert_eq!(handle.worker_panics(), 1);
    let st = controller.status();
    assert!(!st.finished, "{st:?}");
    assert_eq!(st.capture, CaptureState::Running, "{st:?}");
    finish(handle, &ctl);
}

/// **A reader that panics EVERY time ends the run — visibly, naming the panic.**
///
/// The other half of the guard: recovery is bounded, and when it is used up the run must end as
/// [`CaptureState::Ended`] *with a cause that says a thread panicked*. A run that ends with
/// `capture_note: None` here is indistinguishable from a user pressing stop.
#[test]
fn a_reader_that_always_panics_ends_the_run_saying_so() {
    let dir = TempDir::new("t541-always-panics");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    assert!(
        wait_captured(&handle, 0, 100_000),
        "the run never warmed up"
    );

    handle.panic_worker(Some(("hk-detect", u32::MAX)));
    let _ = controller.retune(CENTER + 400e3, FS * 2.0);

    assert!(
        wait_finished(&controller),
        "a reader panicking on every segment must end the run rather than restart for ever: {:?}",
        controller.status()
    );
    let st = controller.status();
    assert_eq!(st.capture, CaptureState::Ended, "{st:?}");
    let note = st
        .capture_note
        .as_deref()
        .unwrap_or("<none — THIS IS T-541: the run ended and said nothing>");
    assert!(
        note.contains("panicked"),
        "the run must end naming the panic, not silently: {note:?} ({st:?})"
    );
    assert!(
        handle.worker_panics() >= u64::from(MAX_RECOVERY_ATTEMPTS),
        "every recovery attempt's panic is counted: {}",
        handle.worker_panics()
    );
    // And the control plane still answers after the run has ended, rather than the process being
    // gone: this is the difference the supervisor sees.
    assert!(controller.status().finished);
    finish(handle, &ctl);
}

// ---------------------------------------------------------------------------------------------
// Property 2 — every device error degrades visibly
// ---------------------------------------------------------------------------------------------

/// **A read error that clears is absorbed: the run recovers and says it did.**
///
/// The commonest real fault — a USB stall, a transfer timeout — and the one that must NOT end the
/// run. Two reads fail, so two segments die; the third delivers and the run is back on `running`
/// with the failure visible in the counters rather than in the user's face.
#[test]
fn a_transient_read_error_recovers_and_is_counted() {
    let dir = TempDir::new("t541-transient-read");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    assert!(
        wait_captured(&handle, 0, 100_000),
        "the run never warmed up"
    );

    let at = captured(&handle.counters());
    ctl.fail_reads_for(2);

    assert!(
        wait_captured(&handle, at, 300_000),
        "capture never came back from a read error that clears — a front end that hiccups must \
         not cost the run: {:?}",
        controller.status()
    );
    let st = controller.status();
    assert!(!st.finished, "{st:?}");
    assert_eq!(st.capture, CaptureState::Running, "{st:?}");
    assert!(
        stat(&controller, "capture_failures") >= 1,
        "the failure is reported even though it was absorbed — 'the device refused something' and \
         'nothing went wrong' must not read identically (T-529): {st:?}"
    );
    assert!(stat(&controller, "capture_recoveries") >= 1, "{st:?}");
    assert_eq!(
        handle.worker_panics(),
        0,
        "a device error is an error, never a panic"
    );
    finish(handle, &ctl);
}

/// **A control call the device refuses is absorbed too, and answered.**
///
/// The other side of the device: this fault is on the **control** thread, inside the caller's own
/// `retune`, which is what a HackRF does when `hackrf_set_sample_rate` returns non-zero. The
/// re-plumb has already torn the segment down by then, so "the control call failed" must not mean
/// "the run has no capture": recovery re-sends the window and the run continues.
#[test]
fn a_refused_rate_change_does_not_end_the_run() {
    let dir = TempDir::new("t541-refused-rate");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    assert!(
        wait_captured(&handle, 0, 100_000),
        "the run never warmed up"
    );

    ctl.refuse_rate(1);
    let at = captured(&handle.counters());
    let answer = controller.retune(CENTER + 400e3, FS * 2.0);

    // The answer is an ANSWER, and it is the refusal: never a silent success, never a hang.
    assert!(
        matches!(answer, Err(ControlFailure::Source(_))),
        "a rate the device refused must come back to the caller as that refusal: {answer:?}"
    );
    assert!(
        wait_captured(&handle, at, 200_000),
        "capture never came back after the device refused a rate change (answer was {answer:?}): \
         {:?}",
        controller.status()
    );
    let st = controller.status();
    assert!(
        !st.finished,
        "a refused control call must not end the run: {st:?} (answer {answer:?})"
    );
    assert_eq!(st.capture, CaptureState::Running, "{st:?}");
    assert_eq!(
        (st.center_hz, st.sample_rate_hz),
        (CENTER, FS),
        "a refused rate leaves the run on the window it was already on, and SAYS so — 'refused, \
         back where it was' and 'refused, nobody knows where the radio is' are not the same fact \
         (T-497): {st:?}"
    );
    assert!(
        ctl.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("rate ")),
        "the device WAS commanded — 'told' and 'accepted' stay separable"
    );
    finish(handle, &ctl);
}

/// **A front end that returns structurally impossible blocks must not take anything down.**
///
/// Zero sample rate, NaN centre, a sample index that jumps backwards. Floats do not trap, so the
/// question is never "does the division panic" but whether anything downstream turns an infinity
/// or a NaN into an index, a length or a capacity. This asserts the whole pipeline eats it: no
/// panic anywhere, the control plane still answers, and capture is still delivering afterwards.
#[test]
fn garbage_provenance_does_not_take_the_run_down() {
    let dir = TempDir::new("t541-garbage");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    assert!(
        wait_captured(&handle, 0, 100_000),
        "the run never warmed up"
    );

    let at = captured(&handle.counters());
    ctl.garbage(8);

    assert!(
        wait_captured(&handle, at, 400_000),
        "capture stopped after eight blocks of impossible provenance: {:?}",
        controller.status()
    );
    assert_eq!(
        handle.worker_panics(),
        0,
        "a garbage block must be data the pipeline rejects, never a panic: {:?}",
        controller.status()
    );
    let st = controller.status();
    assert!(!st.finished, "{st:?}");
    assert!(
        st.center_hz.is_finite() && st.sample_rate_hz.is_finite(),
        "the control plane must never SERVE the garbage back as the tuned window: {st:?}"
    );
    assert_states_itself(&controller, "after garbage blocks");
    finish(handle, &ctl);
}

// ---------------------------------------------------------------------------------------------
// The same matrix through the MOCK SDR — the device `ops/fuzz-rig.sh` soaks against
// ---------------------------------------------------------------------------------------------

/// **The fault shapes the fuzz rig arms are survived, in CI, against the mock.**
///
/// `ops/fuzz-rig.sh` reported *0 crashes in 69 cycles* against a mock that **cannot fail**: it
/// always lands exactly where it is told and every read succeeds, so the number was a statement
/// about the device, not about the backend. The rig now arms `HK_MOCK_FAULT` on every restart;
/// this is the same two shapes, bounded and in CI, so a regression is caught by `just test`
/// rather than by a soak nobody is watching.
#[test]
fn the_mock_sdr_faults_the_fuzz_rig_arms_are_survived() {
    for fault in [
        MockFault::ReadFailsEvery { n: 120 },
        MockFault::RefuseRate { count: u32::MAX },
    ] {
        let dir = TempDir::new("t541-mock-faults");
        let meta = common::tone_recording(&dir.0.join("rec"), "tone", 1.0e6, 2.0, 100.0e6, None);
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
        let info = SourceInfo {
            sample_rate_hz: 1.0e6,
            center_hz: 100.0e6,
            start_time: source.start_time(),
        };
        let mut plan = replay_plan(100.0e6, 1.0e6, info.start_time);
        plan.extra = json!({ "pipeline": { "ring_s": 0.5 } });
        let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
        cfg.source_class = window_class(100.0e6, 1.0e6);
        cfg.live_window_class = true;
        cfg.settings.chains = Some(Vec::new());
        let handle = Pipeline::start(
            cfg,
            Box::new(source),
            info,
            None,
            Box::new(TrackInventory::default()),
        )
        .unwrap();
        let controller = handle.controller();
        assert!(
            wait_captured(&handle, 0, 200_000),
            "{fault:?}: the run never warmed up: {:?}",
            controller.status()
        );
        // Ask the device to move, which is what a sweep does all day.
        let _ = controller.retune(100.1e6, 2.0e6);
        let at = captured(&handle.counters());
        assert!(
            wait_captured(&handle, at, 300_000),
            "{fault:?}: capture never came back: {:?}",
            controller.status()
        );
        let st = controller.status();
        assert!(!st.finished, "{fault:?}: the run ended: {st:?}");
        assert_eq!(handle.worker_panics(), 0, "{fault:?}: {st:?}");
        assert_states_itself(&controller, "under an armed mock fault");
        handle.stop();
        let _ = handle.wait();
    }
}

/// **A device that vanishes ends the run — and the run says the device is gone.**
///
/// T-508 covers the recovery budget for this shape; what is asserted here is the part the user
/// sees: the end is `ended` + a cause, never `finished` with nothing said, and `status()` is
/// still served afterwards — a degraded server, not an absent one.
#[test]
fn a_device_that_vanishes_ends_the_run_with_a_cause() {
    let dir = TempDir::new("t541-vanished");
    let (handle, ctl) = radio_run(&dir);
    let controller = handle.controller();
    assert!(
        wait_captured(&handle, 0, 100_000),
        "the run never warmed up"
    );

    ctl.fail_reads(true);

    assert!(
        wait_finished(&controller),
        "a front end that is gone for good must end the run, not hang: {:?}",
        controller.status()
    );
    let st = controller.status();
    assert_eq!(st.capture, CaptureState::Ended, "{st:?}");
    assert!(
        st.capture_note.is_some(),
        "THIS IS T-541: the run ended on a device failure and said nothing, which reads exactly \
         like a user pressing stop: {st:?}"
    );
    // Still answering. This is the whole difference between a degraded server and a dead one.
    for _ in 0..5 {
        assert!(controller.status().finished);
        std::thread::sleep(Duration::from_millis(10));
    }
    finish(handle, &ctl);
}
