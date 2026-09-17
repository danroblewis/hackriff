//! T-339 / T-347 — **pause freezes the view, and it cannot freeze anything else.**
//!
//! The invariant (CLAUDE.md, "Time, the waterfall, and the live view"): capture, the ring and
//! detection are always-on; the UI's time window is **independent view state**. Pausing never stops
//! or slows the SDR, the ring, or detection — it only changes what the screen shows.
//!
//! T-339 proved the first half against the pause control that existed then: a run-wide
//! `PipelineController::set_paused` behind `POST /api/control/pause`, which stopped the spectrum
//! publisher for the whole run. It left the ring and detection alone, so the invariant held — but
//! "independent view state" did not: one browser pressing Pause froze every other browser's
//! waterfall, because a run-wide boolean cannot represent N viewers.
//!
//! **T-347 removed the lever rather than fixing its scope.** There is no `set_paused`, no
//! `DisplaySettings::paused` and no `/api/control/pause`; holding the view is the client's own time
//! cursor and reaches nothing. So this test now asserts the stronger property at the same seam: the
//! run's rows, ring, capture and detection **all keep advancing across every control a viewing
//! session can still make**, and the frozen-row assertion that used to prove the pause was in force
//! is inverted — rows must never stop.
//!
//! **Why lossless (the gate is on).** The way a view could realistically stop capture is
//! backpressure: a reader that stops advancing its flow-gate cursor would hold the capture thread
//! back once the writer got half a ring ahead of it (`hk_pipeline::gate`). So the run is lossless
//! with a small ring (`ring_s = 0.5`), and each phase pushes several times the gate's slack through
//! it. If any view control stalled the spectrum reader's cursor, the source would stop being read
//! and `wait_emitted` below would time out.
//!
//! **The live control.** Phase A is a plain run and phase B is the same sample budget with the
//! display control driven the way a session drives it (T-067's FFT size, averaging, row rate and
//! window all move). Both are asserted, so the test cannot pass on a run where nothing was
//! happening in the first place.
//!
//! The cross-client half of T-347 — *two* connected browsers, one holding its view, the other still
//! advancing — is asserted where clients actually are, over two real WebSockets:
//! `crates/hk-cli/tests/api_contract.rs`, `one_clients_pause_never_freezes_another_clients_stream`.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use hk_dsp::WindowKind;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::stats::Counters;
use hk_pipeline::{
    DisplayPatch, Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
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
/// Samples to let in-flight rows land after a display change before the next window is measured.
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
fn holding_the_view_cannot_stop_the_runs_rows_the_ring_or_detection() {
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

    // ---- the whole remaining view-control surface, driven the way a session drives it ----
    //
    // T-347: this is now the complete list. A viewer can change the geometry of the published rows
    // and nothing else — there is no control here that stops them.
    let display = handle
        .controller()
        .set_display(&DisplayPatch {
            fft_size: Some(2048),
            averaging: Some(8),
            rows_per_s: Some(10.0),
            window: Some(WindowKind::FlatTop),
        })
        .expect("a display change is a view control");
    assert_eq!((display.fft_size, display.averaging), (2048, 8));
    let settle = ctl.emitted() + SETTLE_SAMPLES;
    assert!(
        ctl.wait_emitted(settle, LIMIT),
        "the source stalled right after the display change"
    );

    // ---- phase B: the view held, the display moved ----
    let b0 = marks(&handle, &counters);
    let target = ctl.emitted() + PHASE_SAMPLES;
    assert!(
        ctl.wait_emitted(target, LIMIT),
        "the source stopped being read while the view was held: a view control reached the device"
    );
    let b = delta(b0, marks(&handle, &counters));
    eprintln!("held:    {b:?}");

    // **The T-347 assertion, and it is the inverse of the one that stood here.** This used to read
    // `b.rows == 0` — proof the run-wide pause was in force. A run-wide pause is exactly what one
    // browser must not be able to do to another, so the property is now that the rows never stop:
    // no control a viewing session can make silences the stream every other viewer is reading.
    assert!(a.rows > 0, "the view never advanced while playing: {a:?}");
    assert!(
        b.rows > 0,
        "the run's rows stopped while the view was held: a viewer still has a lever on the \
         stream every other viewer shares ({b:?}, playing: {a:?})"
    );

    // The property: the ring, the capture thread and detection all carried on while the view was
    // held, by margins comparable to the phase-A control.
    assert!(
        b.captured >= PHASE_SAMPLES,
        "capture slowed while the view was held: {} samples over a phase of {PHASE_SAMPLES} (playing: {})",
        b.captured,
        a.captured
    );
    assert!(
        b.ring >= PHASE_SAMPLES,
        "the ring stopped advancing while the view was held: +{} (playing: +{})",
        b.ring,
        a.ring
    );
    assert!(
        a.detections_written > 0,
        "no detections were written while playing, so the held comparison is empty: {a:?}"
    );
    assert!(
        b.detections_written > 0,
        "detection stopped writing while the view was held: +{} rows (playing: +{})",
        b.detections_written,
        a.detections_written
    );
    assert!(
        b.detections > 0,
        "detection stopped producing records while the view was held: +{} (playing: +{})",
        b.detections,
        a.detections
    );
    // Without this the backpressure leg would be vacuous: a gate that never engaged could not have
    // stalled the source whether or not a reader held its cursor.
    assert!(
        b.gate_waits > 0,
        "the lossless gate never held a block back during the held phase, so this run did not \
         exercise the backpressure path a stalled reader would sit on: {b:?}"
    );

    // ---- and back to the run's own settings: still nothing stops ----
    let display = handle
        .controller()
        .set_display(&DisplayPatch {
            fft_size: Some(1024),
            averaging: Some(1),
            rows_per_s: Some(25.0),
            window: Some(WindowKind::Hann),
        })
        .expect("a display change is a view control");
    assert_eq!(display.averaging, 1);
    let c0 = marks(&handle, &counters);
    let target = ctl.emitted() + PHASE_SAMPLES;
    assert!(
        ctl.wait_emitted(target, LIMIT),
        "the source stalled after the display went back"
    );
    let c = delta(c0, marks(&handle, &counters));
    eprintln!("back:    {c:?}");
    assert!(c.rows > 0, "the rows did not carry on: {c:?}");

    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    eprintln!("{}", summary.to_text());
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}
