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
//!
//! **T-920 made the backpressure leg a demonstration instead of a coincidence.** "No view control
//! stalled the source" is only worth asserting if a stalled reader *could* have stalled it, so the
//! run used to guard against vacuity with `gate_waits > 0` — the gate happened to hold a block
//! back inside the measured window. That is a race between one replay thread and three FFT
//! threads, not a property: on an idle 24-core Linux box, on current `main`, it failed 5 runs in 7
//! while every reader reported `lost 0` and every detection produced was stored. The run now
//! *pins a gate cursor*, watches the capture thread stop within the gate's own documented bound
//! (half a ring plus the block in flight), and releases it and watches the run resume — three
//! facts no scheduler can decide.
//!
//! **T-489 put a viewer in the run.** The producer computes rows only while a consumer is open, so
//! a run with no consumer at all publishes none — the headless-survey saving, not a lever any
//! viewer holds. The claim asserted below is about "the stream every other viewer is reading", so
//! the run now has one: a consumer open from the first offer to the end, across both phases.

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
use hk_stream::{Declared, StreamKind};
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
/// The mock radio's block size, and so the most that can be in flight past a gate decision.
const BLOCK_SAMPLES: u64 = 16_384;

const LIMIT: Duration = Duration::from_secs(120);

/// What the seam reports about capture, detection and the view.
#[derive(Clone, Copy, Debug)]
struct Marks {
    /// Newest ring sample index: the ring advancing.
    ring: u64,
    /// Samples the capture thread wrote: the SDR being read.
    captured: u64,
    /// Detection rows written to the repository: detection still running. **The one mark here
    /// the flow gate does not pin to the capture clock** — it lives on the far side of the detect
    /// writer's channel, so it advances in wall clock (see [`wait_phase`], T-448).
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

/// Waits until **every quantity the phase goes on to assert** has moved past `from`: capture and
/// the ring by `n` samples, and detection by at least one record produced and one row stored.
/// `Err` names the one that did not.
///
/// **T-433. A phase must close on the counters it is asserted on.** Capture and the ring used to
/// be asserted against a phase the SOURCE's `emitted` counter closed, and the three move in
/// lockstep only up to the block in flight between them: `writer.push` advances the ring, then
/// `add(&c.samples, n)` advances capture, and `emitted` ran ahead of both. `wait_emitted` also
/// overshoots its target to the next whole block, so the phase really spanned
/// 153 x 16 384 = 2 506 752 emitted samples against a 2 500 000 bound — **6 752 samples of
/// headroom, less than half a block**. One block in flight at the closing mark and the answer is
/// 2 506 752 - 16 384 = 2 490 368: the figure T-406 and T-436 reported independently, to the
/// sample, which is why it was bit-identical on two trees and not the "load flake" it was filed
/// as. Load makes the in-flight block likelier; it does not make the bound sound.
///
/// **T-448, the same rule reaching one counter further.** `detections_written` was left outside
/// this wait and asserted `> 0` over a window this wait closes in *captured samples*, and it is
/// the **one asserted counter the lossless gate does not pin to the capture clock**. Every other
/// one is pinned: the spectrum and detect readers hold `GateCursor`s, so `hk_pipeline::gate` stops
/// the capture thread once a block would run more than half a ring (here 125 000 samples) past
/// the slowest of them — a 2 500 000-sample phase therefore *forces* ~120 spectrum rows and the
/// detection records that go with them. `detections_written` is on the far side of the detect
/// writer's channel, which no cursor and no gate reaches: it advances when that thread is
/// scheduled and gets the repository lock, i.e. in **wall clock**, while the phase closes in
/// capture time that an unpaced replay runs at roughly 27x real time. A 2 500 000-sample phase is
/// ~5 s of capture and ~0.2 s of wall clock, so the writer missing its slot costs the whole
/// phase's writes and the next phase gets them: T-436's A/B recorded exactly that, `playing` with
/// `detections_written: 0` against `detections: 6`, and `held` with 8 — the two phases' rows,
/// stored in one pass, in the second phase's window.
///
/// So the phase now waits for detection too. The claim is unchanged and none of it moves into this
/// wait's tolerance: capture, the ring and detection all had to advance for the phase to close at
/// all, and a view control that stopped any of them blows the 120 s deadline with a message naming
/// which one — instead of being scored against a counter whose clock the phase never controlled.
fn wait_phase(
    handle: &PipelineHandle,
    c: &Counters,
    from: Marks,
    n: u64,
    limit: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + limit;
    loop {
        let d = delta(from, marks(handle, c));
        let stalled = if d.captured < n {
            "the capture thread"
        } else if d.ring < n {
            "the ring"
        } else if d.detections == 0 {
            "detection (no record produced)"
        } else if d.detections_written == 0 {
            "detection's writer (no row stored)"
        } else {
            return Ok(());
        };
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "{stalled} stopped advancing: {d:?} over a phase of {n} samples, after {limit:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
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
    let (radio, ctl) = radio::Radio::new(
        CENTER,
        FS,
        BLOCK_SAMPLES as usize,
        radio::tone(|_| OFFSET_HZ),
    );
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true; // the flow gate is live: a stalled reader would stop the source
    cfg.settings.chains = Some(Vec::new());
    // **A viewer, because the claim is about one (T-489).** The rows this test requires never to
    // stop are "the stream every other viewer is reading", and since T-489 the producer computes
    // them only while a consumer is open — an unwatched run publishes none, which is a saving, not
    // a viewer's lever. So the test attaches the viewer whose stream it is asserting about. The
    // consumer is a sink that drops what it reads: nothing here inspects the rows, only that they
    // keep coming. It stays open for the whole run, so no phase below is measured unwatched.
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.kind == StreamKind::Spectrum {
            handle
                .subscribe(
                    "t347-viewer",
                    Declared::local(std::io::sink()),
                    Box::new(|_| {}),
                )
                .expect("a viewer subscribed to the spectrum stream");
        }
    }));
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
    if let Err(why) = wait_phase(&handle, &counters, a0, PHASE_SAMPLES, LIMIT) {
        panic!("the run stalled while playing: {why}");
    }
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
    if let Err(why) = wait_phase(&handle, &counters, b0, PHASE_SAMPLES, LIMIT) {
        panic!(
            "the run stalled while the view was held, so a view control reached past the view: \
             {why}"
        );
    }
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
    // held, by margins comparable to the phase-A control. All four counters are now the phase's own
    // closing condition (`wait_phase`), so these restate what the wait established rather than
    // racing it: a view control that reached the device stops capture, and the wait's deadline —
    // 120 s against 5 s of samples — is what fails, not a comparison one in-flight block decides
    // (T-433) or one the detect writer's scheduling decides (T-448). The same lines are asserted
    // for the playing control, so neither phase can pass on a run where nothing was happening.
    assert!(
        a.captured >= PHASE_SAMPLES && a.ring >= PHASE_SAMPLES,
        "{a:?}"
    );
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
    // The backpressure leg is not vacuous — but it is **demonstrated below**, after the view
    // phases, rather than asserted here (T-920). What stood here was `b.gate_waits > 0`: proof
    // that the gate happened to hold a block back inside this 5-second window. Whether the writer
    // runs a half ring ahead of three FFT readers inside any particular window is a race between
    // threads, not a property of the run: on one idle 24-core Linux box, on current `main`, that
    // assertion failed 5 runs in 7 (`gate_waits: 0` in both phases) while every reader reported
    // `lost 0` and the run stored every detection it produced. It is the second kind of
    // timing-based test in docs/10 §3.6 — an assertion that is really a throughput bound — and
    // the fix is not to move it to the `timing` tier but to stop needing the coincidence: see
    // `a stalled reader really does stop the source` below, which pins a cursor and watches the
    // capture thread stop, in a way no scheduler can make pass or fail by luck.

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

    // ---- the claim's other half: a stalled READER really does stop the source (T-920) ----
    //
    // Everything above says a view control did not stop capture. That is only worth asserting if
    // something *could* have: if the lossless gate were not armed, no view control could stall
    // the source however badly it behaved, and every phase above would pass for the wrong reason.
    //
    // So the run demonstrates it, instead of waiting to catch the gate in the act. A cursor is
    // registered claiming everything from the current live edge on and then never advanced — a
    // reader that has stopped reading, which is precisely the shape of backpressure a view
    // control would have to produce to reach the device. `FlowGate` admits a block only while it
    // stays within `slack` (half the ring) of the slowest claim, so the capture thread must stop
    // after at most `slack` further samples plus the block in flight. That bound holds whatever
    // the relative speed of the writer and the readers is, which is exactly what `gate_waits > 0`
    // did not.
    let gate = handle
        .flow_gate()
        .expect("a running segment has a flow gate");
    assert!(
        gate.enabled(),
        "this run is configured lossless, so its gate must be live"
    );
    let bound = gate.slack() + 2 * BLOCK_SAMPLES;
    let d0 = marks(&handle, &counters);
    // Read off the gate itself, not `counters.source.gate_waits`: the capture thread publishes
    // that counter only *after* the block it waited for is pushed (`capture.rs`), so a thread
    // currently parked in `wait_for_block` has not reported the wait it is sitting in — which is
    // the one state this block is about.
    let waits_before = gate.waits();
    let stalled = gate.register(handle.ring_position());
    // Poll until the capture thread is still, failing the moment it runs past the bound.
    let deadline = std::time::Instant::now() + LIMIT;
    let mut last = 0u64;
    let held = loop {
        let d = delta(d0, marks(&handle, &counters));
        assert!(
            d.captured <= bound,
            "a reader that stopped reading did NOT stop the source: capture advanced {} samples \
             past a pinned cursor, past the gate's own bound of {bound} (slack {} + two blocks). \
             The lossless gate is not holding the capture thread, so nothing above about view \
             controls not stalling it means anything: {d:?}",
            d.captured,
            gate.slack()
        );
        if d.captured > 0 && d.captured == last {
            break d;
        }
        last = d.captured;
        assert!(
            std::time::Instant::now() < deadline,
            "the capture thread never settled against the pinned cursor: {d:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let waits = gate.waits() - waits_before;
    eprintln!(
        "stalled: {held:?} (gate slack {}, waits +{waits})",
        gate.slack()
    );
    assert!(
        waits > 0,
        "the source stopped, but the gate recorded no wait — so something OTHER than the \
         backpressure path stopped it, and this is not the demonstration it claims to be: \
         {held:?}"
    );

    // ---- and releasing it starts the source again: the stall was the cursor, not the run ----
    //
    // Without this the block above could pass on a run that had simply ended.
    // The claim here is narrower than a phase's, and so is the wait: capture and the ring move
    // again, by more than the whole bound they were held at. Detection is deliberately NOT waited
    // on — it is the one counter the gate does not pin to the capture clock (see [`wait_phase`],
    // T-448), and every phase above has already established it keeps running.
    drop(stalled);
    let r0 = marks(&handle, &counters);
    let deadline = std::time::Instant::now() + LIMIT;
    let r = loop {
        let r = delta(r0, marks(&handle, &counters));
        if r.captured > bound && r.ring > bound {
            break r;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the run did not resume once the stalled reader released its cursor: {r:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    eprintln!("resumed: {r:?}");

    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    eprintln!("{}", summary.to_text());
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    // **T-448.** The detect writer keeps its own cadence — it is the one counter above that the
    // flow gate does not pin to the capture clock, which is why a phase may not score it — but
    // nothing may be *lost* to it. Every detection the run produced is a row in the repository by
    // the time the run closes. That is the guarantee the per-phase counter was being read as, and
    // this is the seam where it actually holds: `detections_stored` is a `SELECT count(*)`, not a
    // counter, so it also checks the rows are really there.
    let produced = counters.detect.detections.load(Ordering::Relaxed);
    assert_eq!(
        summary.detections_stored, produced,
        "the run produced {produced} detections and stored {}: the writer dropped work rather \
         than deferring it",
        summary.detections_stored
    );
}
