//! T-339 — **pause freezes the view, not the capture.**
//!
//! The invariant (CLAUDE.md, "Time, the waterfall, and the live view"): capture, the ring and
//! detection are always-on; the UI's time window is independent view state. Pausing never stops or
//! slows the SDR, the ring, or detection — it only changes what the screen shows.
//!
//! The property is asserted at the seam it lives at, not in the UI: a UI test would only prove that
//! today's button calls no stopping function, while this proves that the one thing the pause
//! control *does* reach — `PipelineController::set_paused`, the same call behind
//! `POST /api/control/pause` — leaves the ring advancing and detections being written.
//!
//! **Why lossless (the gate is on).** The way a paused view could realistically stop capture is
//! backpressure: a reader that stops advancing its flow-gate cursor while paused would hold the
//! capture thread back once the writer got half a ring ahead of it (`hk_pipeline::gate`). So the run
//! is lossless with a small ring (`ring_s = 0.5`), and each phase pushes several times the gate's
//! slack through it. If pausing stalled the spectrum reader's cursor, the source would stop being
//! read and `wait_emitted` below would time out.
//!
//! **The live control.** A paused phase that advances proves nothing unless an unpaused phase of
//! the same size advances too — otherwise the test would pass on a run where nothing was happening
//! in the first place. So phase A runs playing and phase B runs paused, over the same sample budget,
//! and both are asserted. The frozen spectrum row count across phase B is the third leg: it proves
//! the pause was actually in force, so "capture carried on" is not just "the pause never applied".

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::stats::Counters;
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use serde_json::json;

const CENTER: f64 = 433.92e6;
const FS: f64 = 500e3;
const OFFSET_HZ: f64 = 80e3;

/// A short ring, so the lossless gate's slack (half of it) is small and cheap to overrun.
const RING_S: f64 = 0.5;
/// Samples per phase: 5 s, twenty times the gate's slack at `RING_S`, and long enough to span
/// several detection writes (this scene produces roughly one per second of sample time).
const PHASE_SAMPLES: u64 = 2_500_000;
/// Samples to let in-flight rows land after a pause before the frozen window is measured.
const SETTLE_SAMPLES: u64 = 50_000;

const LIMIT: Duration = Duration::from_secs(120);

/// What the seam reports about capture, detection and the view.
#[derive(Clone, Copy, Debug)]
struct Marks {
    /// Newest ring sample index: the ring advancing.
    ring: u64,
    /// Samples the capture thread wrote: the SDR being read.
    captured: u64,
    /// Detection rows written to the repository: detection still running.
    detections_written: u64,
    /// Detection records produced.
    detections: u64,
    /// Spectrum rows the publisher produced: the view advancing.
    rows: u64,
    /// Blocks the lossless gate held back: evidence the backpressure path was exercised at all.
    gate_waits: u64,
}

fn marks(handle: &PipelineHandle, c: &Counters) -> Marks {
    Marks {
        ring: handle.ring_position(),
        captured: c.source.samples.load(Ordering::Relaxed),
        detections_written: c.detect.detections_written.load(Ordering::Relaxed),
        detections: c.detect.detections.load(Ordering::Relaxed),
        rows: c.spectrum.rows.load(Ordering::Relaxed),
        gate_waits: c.source.gate_waits.load(Ordering::Relaxed),
    }
}

/// `after - before`, per field.
fn delta(before: Marks, after: Marks) -> Marks {
    Marks {
        ring: after.ring.saturating_sub(before.ring),
        captured: after.captured.saturating_sub(before.captured),
        detections_written: after
            .detections_written
            .saturating_sub(before.detections_written),
        detections: after.detections.saturating_sub(before.detections),
        rows: after.rows.saturating_sub(before.rows),
        gate_waits: after.gate_waits.saturating_sub(before.gate_waits),
    }
}

#[test]
fn a_paused_view_keeps_the_ring_advancing_and_detections_being_written() {
    let dir = TempDir::new("view-pause-keeps-capture");
    let (radio, ctl) = radio::Radio::new(CENTER, FS, 16_384, radio::tone(|_| OFFSET_HZ));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true; // the flow gate is live: a stalled reader would stop the source
    cfg.settings.chains = Some(Vec::new());
    let handle = Pipeline::start(
        cfg,
        Box::new(radio),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters: Arc<Counters> = handle.counters();

    // Warm-up: the detector needs a floor before it writes anything, so the phases below measure a
    // pipeline already in its steady state rather than its first second.
    let warm = ctl.emitted() + 2 * PHASE_SAMPLES;
    assert!(ctl.wait_emitted(warm, LIMIT), "the run never warmed up");
    let deadline = std::time::Instant::now() + LIMIT;
    while counters.detect.detections_written.load(Ordering::Relaxed) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no detection was written before the phases began: the fixture is not exercising \
             detection, so nothing below would mean anything"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // ---- phase A: playing (the live control) ----
    let a0 = marks(&handle, &counters);
    let target = ctl.emitted() + PHASE_SAMPLES;
    assert!(
        ctl.wait_emitted(target, LIMIT),
        "the source stalled while playing"
    );
    let a = delta(a0, marks(&handle, &counters));
    eprintln!("playing: {a:?}");

    // ---- pause ----
    let display = handle.controller().set_paused(true);
    assert!(display.paused, "the controller did not report the pause");
    let settle = ctl.emitted() + SETTLE_SAMPLES;
    assert!(
        ctl.wait_emitted(settle, LIMIT),
        "the source stalled right after the pause"
    );

    // ---- phase B: paused ----
    let b0 = marks(&handle, &counters);
    let target = ctl.emitted() + PHASE_SAMPLES;
    assert!(
        ctl.wait_emitted(target, LIMIT),
        "the source stopped being read while the view was paused: pausing reached the device"
    );
    let b = delta(b0, marks(&handle, &counters));
    eprintln!("paused:  {b:?}");

    // The view really was frozen — without this, "capture carried on" could just mean the pause
    // never took effect.
    assert_eq!(
        b.rows, 0,
        "the view was not frozen: {} spectrum rows were published while paused",
        b.rows
    );
    assert!(a.rows > 0, "the view never advanced while playing: {a:?}");

    // The property: the ring, the capture thread and detection all carried on while paused, by
    // margins comparable to the unpaused control.
    assert!(
        b.captured >= PHASE_SAMPLES,
        "capture slowed while paused: {} samples over a phase of {PHASE_SAMPLES} (playing: {})",
        b.captured,
        a.captured
    );
    assert!(
        b.ring >= PHASE_SAMPLES,
        "the ring stopped advancing while paused: +{} (playing: +{})",
        b.ring,
        a.ring
    );
    assert!(
        a.detections_written > 0,
        "no detections were written while playing, so the paused comparison is empty: {a:?}"
    );
    assert!(
        b.detections_written > 0,
        "detection stopped writing while the view was paused: +{} rows (playing: +{})",
        b.detections_written,
        a.detections_written
    );
    assert!(
        b.detections > 0,
        "detection stopped producing records while the view was paused: +{} (playing: +{})",
        b.detections,
        a.detections
    );
    // Without this the backpressure leg would be vacuous: a gate that never engaged could not have
    // stalled the source whether or not a paused reader held its cursor.
    assert!(
        b.gate_waits > 0,
        "the lossless gate never held a block back during the paused phase, so this run did not \
         exercise the backpressure path a paused reader would stall: {b:?}"
    );

    // ---- resume: the view starts again, which is what "freezes the view" means ----
    assert!(!handle.controller().set_paused(false).paused);
    let c0 = marks(&handle, &counters);
    let target = ctl.emitted() + PHASE_SAMPLES;
    assert!(
        ctl.wait_emitted(target, LIMIT),
        "the source stalled after resuming"
    );
    let c = delta(c0, marks(&handle, &counters));
    eprintln!("resumed: {c:?}");
    assert!(c.rows > 0, "the view did not resume: {c:?}");

    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    eprintln!("{}", summary.to_text());
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}
