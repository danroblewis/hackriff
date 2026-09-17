//! T-343 — **the retune is the one control that stops the world, and it is the only one.**
//!
//! Sibling of `view_pause_keeps_capture.rs` (T-339). That test pins the view side: pausing never
//! stops the capture. This one pins the asymmetry it implies — exactly one control plane method
//! *does* reach the front end and stop the running segment, and every other one leaves it alone.
//!
//! The defect this guards is the one T-339's audit found. `ui/src/controls/gestures.ts` fired
//! `hooks.overflow(...)` when an accumulated pan exceeded 5 % of the view width; that was wired to
//! `POST /api/control/center`, which reaches [`PipelineController::retune`], which sets
//! `shared.stop.store(true)` and re-plumbs the run whenever the window's class or sample rate
//! changes. **A pan let go slightly too far could stop and restart capture.** No type, gesture or
//! confirmation separated the two; a 5 % threshold did.
//!
//! So the property has two halves, and the second is the one that matters:
//!
//! 1. A retune across a rate boundary **does** re-plumb: the segment number advances and the
//!    controller reports `replumbed`. This is the one legitimate world-stopper.
//! 2. Every other controller call — display settings, recording start and stop, in the loop a
//!    held-and-scrubbed UI actually makes — leaves the segment number **exactly** where it was and
//!    leaves the front end at exactly the window it was on. Without this half the property could
//!    be satisfied by making the retune unreachable altogether.
//!
//! T-347 shortened that list: there is no `set_paused` to call any more. Holding the view is the
//! client's own time window, so the *only* controller calls a paused, scrubbing, zooming session
//! makes are the ones below.
//!
//! It drives a scripted receiver behind the **generic device contract** (`tests/support/radio.rs`,
//! the same source T-339's test uses), which records every receive-side command it is given. So
//! "reached the device" is not inferred from the controller's own bookkeeping: it is what the front
//! end was actually told.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::time::Duration;

use common::*;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::{
    DisplayPatch, Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan,
};
use serde_json::json;

const CENTER: f64 = 433.92e6;
const FS: f64 = 500e3;
const OFFSET_HZ: f64 = 80e3;
const RING_S: f64 = 0.5;
/// Samples to let the run settle between control calls.
const SETTLE_SAMPLES: u64 = 50_000;
const LIMIT: Duration = Duration::from_secs(120);

#[test]
fn only_a_retune_stops_the_running_segment() {
    let dir = TempDir::new("retune-is-the-one-device-action");
    let (rx, ctl) = radio::Radio::new(CENTER, FS, 16_384, radio::tone(|_| OFFSET_HZ));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
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
    let controller = handle.controller();

    // Let the run reach a steady state before anything is asked of it.
    let warm = ctl.emitted() + 500_000;
    assert!(ctl.wait_emitted(warm, LIMIT), "the run never warmed up");

    let before = controller.status();
    assert!(before.live, "a fixed-window run could not retune at all");
    let device_calls_before = ctl.calls.lock().unwrap().len();

    // ---- the view side: everything a held, scrubbing, zooming UI does ----
    //
    // Each of these is a legitimate UI action that must never reach the front end. They run in the
    // order a session makes them, several times, so a single unlucky ordering is not what passes.
    for round in 0..3 {
        controller
            .set_display(&DisplayPatch {
                fft_size: Some(if round % 2 == 0 { 2048 } else { 1024 }),
                rows_per_s: Some(if round % 2 == 0 { 10.0 } else { 25.0 }),
                ..DisplayPatch::default()
            })
            .expect("a display change is a view control");
        let target = ctl.emitted() + SETTLE_SAMPLES;
        assert!(
            ctl.wait_emitted(target, LIMIT),
            "the source stopped being read while the view was held (round {round})"
        );
        let rec = controller
            .start_recording(Some("view-side"), Some(0.2))
            .expect("recording the tuned window is an output control, not a device action");
        assert!(rec.active);
        let _ = controller.stop_recording();

        let now = controller.status();
        assert_eq!(
            now.segment, before.segment,
            "a view control re-plumbed the run (round {round}): segment {} -> {}",
            before.segment, now.segment
        );
        assert_eq!(
            (now.center_hz, now.sample_rate_hz),
            (before.center_hz, before.sample_rate_hz),
            "a view control moved the front end (round {round})"
        );
    }
    let after_view = ctl.calls.lock().unwrap().clone();
    assert_eq!(
        after_view.len(),
        device_calls_before,
        "a view control reached the device: {:?}",
        &after_view[device_calls_before..]
    );

    // ---- the device side: the one control that legitimately stops the world ----
    let out = controller
        .retune(CENTER, 2.0 * FS)
        .expect("a rate change is a device action the run accepts");
    assert!(
        out.replumbed,
        "a rate change must re-plumb: it is the case that stops and restarts the segment"
    );
    let after = controller.status();
    assert!(
        after.segment > before.segment,
        "the retune did not start a new segment: {} -> {}",
        before.segment,
        after.segment
    );
    assert_eq!(after.sample_rate_hz, 2.0 * FS);
    // The device interface saw it: the rate change was a real command to the front end, not a
    // change of what is drawn.
    let calls = ctl.calls.lock().unwrap().clone();
    assert!(
        calls.len() > device_calls_before,
        "the retune reached no device command: {calls:?}"
    );
    assert!(
        calls[device_calls_before..]
            .iter()
            .any(|c| c.starts_with("rate ")),
        "the front end was never told the new rate: {:?}",
        &calls[device_calls_before..]
    );

    // And the run is still running afterwards: a device action stops the segment, not the session.
    let target = ctl.emitted() + SETTLE_SAMPLES;
    assert!(
        ctl.wait_emitted(target, LIMIT),
        "the run did not resume capture after the retune"
    );

    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}
