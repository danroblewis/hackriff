//! T-446 — **a retune must not stop the spectrum-history pyramid.**
//!
//! Found by T-437's unified-surface spike (docs/16 §8.5a, finding F4) and measured here. On a
//! dedicated mock backend, one retune from 100.8 MHz to 433.92 MHz left the system in a state that
//! looks impossible from the outside: the IQ ring stayed healthy (`t1_s` tracking wall clock,
//! `dropped_samples: 0`), the record-derived coverage plane reported the new centre `observed`
//! with `duty: 1.0`, and `/api/timeline` served `grid.observed_cells = 0` there — for ever.
//! Retuning *back* recovered nothing. The system knew it was looking, stored the samples, and
//! wrote no measurements.
//!
//! **The cause** is in [`hk_pipeline`]'s history reader, at the end of a segment. A re-plumb
//! (T-050) ends the segment's history thread and starts a new one on the *same* `FloorProduct`, so
//! the thread must not seal: [`hk_store::Pyramid::seal_through`] advances a **monotonic**
//! watermark, and `Pyramid::ingest` answers [`hk_store::IngestOutcome::Late`] for any frame whose
//! level-0 block ends at or before it. The guard for that was written
//! `!continues && last_end > 0 || frames_ingested > 0`, which Rust groups as
//! `(!continues && last_end > 0) || frames_ingested > 0` — and `frames_ingested` is a **run-wide**
//! counter shared by every segment. Once any segment had folded one frame the second disjunct was
//! true for ever, so every re-plumb sealed anyway, through *the last frame plus an hour*. One
//! retune therefore stopped spectrum history for an hour of capture time.
//!
//! **Why it mattered enough to be its own ticket:** docs/16 §8.4 makes pan-to-untuned → retune the
//! primary way to move around the unified canvas, and §8's central claim is that "live is the
//! finest growing edge". After the first retune that claim was false.
//!
//! **What this test asserts**, driving a scripted receiver behind the generic device contract
//! (`tests/support/radio.rs`) so the retune is a real device action and not the controller's own
//! bookkeeping:
//!
//! 1. history accumulates for the opening centre (the control — without it the rest is vacuous);
//! 2. after a retune that re-plumbs, history accumulates for the **new** centre;
//! 3. after a retune **back**, the original band resumes accumulating (a fresh seal would have
//!    poisoned it too, and the watermark never retreats);
//! 4. no frame is ever counted `late` — the direct reading of the defect's mechanism, which is
//!    what told the difference between "nothing was written" and "nothing was measured".
//!
//! Each of 1–3 is scoped to the **capture time** of the phase that must have recorded it, for the
//! reason [`observed_cells`] gives: the defect is an hour wide, and a scripted radio read on demand
//! outruns an hour in about a minute of wall clock.
//!
//! Run against the mock/scripted device. Unverified on real hardware, but the mechanism is in the
//! pipeline's own segment plumbing and is independent of the front end.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::{FreqRange, TimeRange, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::{FloorProduct, RegionQuery, Resolution};
use serde_json::json;

/// FM broadcast: `unrestricted`.
const CENTER_A: f64 = 100.8e6;
/// 433 MHz ISM: `metadata-only`. A different content class, so a retune here re-plumbs the run —
/// the same pair of centres the spike's controlled experiment used.
const CENTER_B: f64 = 433.92e6;
const FS: f64 = 500e3;
const OFFSET_HZ: f64 = 80e3;
const BLOCK: usize = 16_384;
const RING_S: f64 = 0.5;
/// Samples of stream time each phase runs for: 4 s at 500 kS/s, which is ~40 history frames at the
/// default 10 rows/s — far more than the handful of level-0 cells each assertion needs.
const PHASE_SAMPLES: u64 = 2_000_000;
/// Capture-time width of every pyramid query: twice a phase, so the window a phase's frames land
/// in is covered with slack for the re-plumb's settle gap, and nothing later than that is.
const WINDOW_NS: i64 = 2 * (PHASE_SAMPLES as i64) * 1_000_000_000 / (FS as i64);
const LIMIT: Duration = Duration::from_secs(180);

/// Observed level-0 cells the pyramid holds for `center` over the **capture-time** window
/// `[from_ns, from_ns + WINDOW_NS)`.
///
/// Level 0 deliberately: the defect is that nothing is *written*, and a coarser tier could answer
/// from a fold of what an earlier segment wrote. Open (unsealed) tiles are readable, so this sees
/// the live edge rather than only what a seal has flushed.
///
/// **The window is the load-bearing part, and the first draft of this test got it wrong.** Asking
/// "has anything for this centre landed *yet*" is patient in wall clock and unbounded in capture
/// time, and the defect is bounded in capture time: the bad seal put the watermark one hour past
/// the last frame, so a run whose stream time eventually passes that hour starts folding again. A
/// scripted radio read on demand covers an hour of stream time in about a minute of wall clock, so
/// the broken build *passed* two of these assertions after 88 s — it had simply outrun the
/// watermark. On the real thing an hour is an hour. Scoping each query to the stream time just
/// after the gesture that must not have broken anything is what makes the assertion mean "history
/// for the band it was tuned to, at the time it was tuned there".
fn observed_cells(product: &Arc<Mutex<FloorProduct>>, center: f64, from_ns: i64) -> usize {
    let p = product.lock().unwrap();
    let h = p
        .uncalibrated_pyramid()
        .query(&RegionQuery {
            freq: FreqRange::centered(center, 0.5 * FS),
            time: TimeRange::new(
                Timestamp::from_unix_nanos(from_ns),
                Timestamp::from_unix_nanos(from_ns + WINDOW_NS),
            ),
            resolution: Resolution::Level(0),
        })
        .expect("the pyramid answered the query");
    h.cells.iter().filter(|c| c.observed()).count()
}

#[test]
fn spectrum_history_keeps_recording_across_a_retune() {
    let dir = TempDir::new("history-survives-retune");
    let (rx, ctl) = radio::Radio::new(CENTER_A, FS, BLOCK, radio::tone(|_| OFFSET_HZ));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER_A, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER_A, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    let handle = Pipeline::start(
        cfg,
        Box::new(rx),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER_A,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let controller = handle.controller();
    let counters = handle.counters();
    let product = handle.floor_product();

    // The capture time the run has reached — the near edge of the next phase's query window.
    let stream_now = || {
        counters
            .stream_time_ns
            .load(Ordering::Relaxed)
            .max(radio::T0_NS)
    };
    // Runs one phase: mark the capture time, let the radio deliver a phase of samples, then wait
    // (patiently in wall clock, because ingest is asynchronous) for the pyramid to hold at least
    // one observed level-0 cell for `center` **inside that phase's capture window**.
    let phase = |what: &str, center: f64, mark: i64| {
        assert!(
            ctl.wait_emitted(ctl.emitted() + PHASE_SAMPLES, LIMIT),
            "the run stopped delivering samples during {what}"
        );
        let deadline = Instant::now() + LIMIT;
        loop {
            let n = observed_cells(&product, center, mark);
            if n > 0 {
                return n;
            }
            assert!(
                Instant::now() < deadline,
                "no spectrum history for {what} at {:.3} MHz over the {:.0} s of capture from \
                 {mark} (ingested {}, LATE {}, tiles {})",
                center / 1e6,
                WINDOW_NS as f64 / 1e9,
                counters.history.frames_ingested.load(Ordering::Relaxed),
                counters.history.frames_late.load(Ordering::Relaxed),
                counters.history.tiles_written.load(Ordering::Relaxed),
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    // ---- 1. the control: the opening centre records at all ----
    //
    // Without this the rest could pass on a run that never wrote anything anywhere.
    phase("the opening centre", CENTER_A, stream_now());

    // ---- 2. the retune, and the property the defect broke ----
    let out = controller
        .retune(CENTER_B, FS)
        .expect("a content-class change is a device action the run accepts");
    assert!(
        out.replumbed,
        "this pair of centres must re-plumb: an in-place tune never ends the history thread, \
         so it could not exercise the defect"
    );
    phase("the NEW centre after a retune", CENTER_B, stream_now());

    // ---- 3. and a retune BACK resumes the original band ----
    //
    // A watermark never retreats, so a segment end that sealed would have poisoned this band too —
    // and the spike measured exactly that: 0 / 32 observed cells, 45 s after retuning back. The
    // window starts at the return, so the cells phase 1 wrote cannot answer for it.
    let out = controller
        .retune(CENTER_A, FS)
        .expect("retuning back is the same device action");
    assert!(out.replumbed, "the return trip must re-plumb too");
    phase(
        "the ORIGINAL band after retuning back",
        CENTER_A,
        stream_now(),
    );

    // ---- 4. the mechanism itself: nothing was ever folded behind a watermark ----
    //
    // Cell counts say the data arrived; this says *why*. A run that never seals mid-flight has no
    // reason to reject a single frame as late, and every frame the defect ate was counted here.
    let late = counters.history.frames_late.load(Ordering::Relaxed);
    let ingested = counters.history.frames_ingested.load(Ordering::Relaxed);
    assert_eq!(
        late,
        0,
        "{late} of {} history frames were folded behind the watermark: a segment end sealed a run \
         that continues",
        late + ingested
    );

    ctl.finish();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}
