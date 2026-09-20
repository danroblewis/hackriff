//! **T-525 — a segment never waits out the settle bound for a window a later command replaced.**
//!
//! The user: *"a full 1 MHz–6000 MHz sweep at 1 s dwell on the live HackRF crashes `hk serve`"* —
//! it goes unhealthy and the demo watcher restarts it. The same sweep on the mock is fine.
//!
//! **Nothing panics.** Measured on the live radio for forty minutes under that sweep with the
//! canvas driven at its finest level: no panic, no abort, no poisoned lock, `hk serve` answering
//! `/` with 200 the whole time. What dies is **`/ws/spectrum/live`**, which answers `410 Gone`
//! ("stream finished") in bursts of several seconds — and `ops/stage.sh`'s health check is `/` plus
//! that handshake, so several seconds of 410 *is* "the server crashed" as the user sees it.
//!
//! **The mechanism.** Every place that moves the front end posts to [`hk_core::ControlMailbox`],
//! which **coalesces**: a change posted behind one the capture thread has not taken yet replaces
//! it. A re-plumb, though, starts its new segment with a [`hk_pipeline`] `WindowGuard` holding the
//! window it *asked* for, and the guard drops every block until a block reports that exact window.
//! If a second command lands before the capture thread drains the mailbox — the next step of a 1 s
//! sweep, or the user pressing Retune at the finest zoom — the first window is never applied and
//! never reported, so the guard drops **everything** until T-497's `WINDOW_SETTLE_TIMEOUT`. For
//! those five seconds the ring gets nothing, the spectrum stage publishes no row, its publisher is
//! finished with no successor, and `spectrum/live` answers 410 to every subscriber.
//!
//! Measured on the live HackRF, two minutes of a 1 MHz–6 GHz sweep at 1 s dwell with the canvas at
//! level 0/0: `blocks_dropped_window` 1912, `window_settle_timeouts` 9, 92 websocket closes; on the
//! mock over the same sweep, 0, 0 and 0. The probe that found it printed
//! `asked 140500000 Hz / 2000000 Hz, block is 3000000000 Hz / 2000000 Hz` — the segment waiting for
//! the sweep's step while the radio sat on the centre the *next* command had sent it to.
//!
//! **This is not T-497 again.** There the front end takes a command and does not move, and the
//! bound is the right answer: something is wrong with the radio and five seconds is the price of
//! finding out. Here the front end did exactly as it was told — it is *the run's own record* of
//! what it asked for that is stale, and no amount of waiting can make a superseded window arrive.
//! T-497's bound stays; what this removes is a wait that was never going to end any other way.
//!
//! **Non-vacuity.** With the supersede check removed from `WindowGuard::admit` (the T-525 block
//! deleted, so `expect` is the snapshot taken at segment start again), this test fails: capture
//! does not resume inside the bound, `window_settle_timeouts` reaches 1, and the failure message
//! prints both.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::stats::Counters;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use serde_json::json;

const CENTER: f64 = 433.92e6;
const FS: f64 = 500e3;
/// The re-plumb's target: a different rate, so the retune re-plumbs and the guard is armed.
const REPLUMB_RATE: f64 = FS * 2.0;
/// Where the *second* command sends the radio. Same class and rate as the re-plumb's window, so it
/// is an in-place tune — the shape a sweep's next step and the canvas's Retune both take.
const SUPERSEDE_HZ: f64 = CENTER + 1.2e6;
const RING_S: f64 = 0.5;

/// How long capture may stay silent before this test calls it a failure.
///
/// Deliberately **shorter than** `WINDOW_SETTLE_TIMEOUT` (5 s) and longer than any honest settle:
/// the claim is "the wait ends because the guard notices the supersede", not "the wait ends
/// eventually". A bound at or past the timeout would pass on the broken code too.
const RESUME_LIMIT: Duration = Duration::from_secs(3);
const WARMUP_LIMIT: Duration = Duration::from_secs(60);

/// Samples that got **past** the window guard into the ring — "the live view has something to
/// draw". The radio's own `emitted` moves whether or not a block is admitted, which is the whole
/// difference this test is about.
fn captured(c: &Counters) -> u64 {
    c.source.samples.load(Ordering::Relaxed)
}

fn stat(controller: &hk_pipeline::PipelineController, key: &str) -> u64 {
    controller.status().stats[key].as_u64().unwrap_or(0)
}

#[test]
fn a_window_superseded_before_it_arrives_does_not_silence_capture_for_the_settle_bound() {
    let dir = TempDir::new("t525-retune-superseded-window");
    let (rx, ctl) = radio::Radio::new(CENTER, FS, 16_384, radio::tone(|_| 80e3));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    // The guard under test is only wrapped on a live, window-classed run — what `hk serve` uses.
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
    let controller = handle.controller();
    let counters: Arc<Counters> = handle.counters();

    assert!(
        ctl.wait_emitted(ctl.emitted() + 500_000, WARMUP_LIMIT),
        "the run never warmed up"
    );
    assert!(
        captured(&counters) > 0,
        "nothing was captured before the retune"
    );

    // ---- the control path is slower than a block, so the mailbox coalesces ----
    //
    // Long enough to cover the re-plumb's own teardown and restart, so the second command is
    // certain to be posted before the first is taken. Without this the two commands would be
    // taken separately and there would be no supersede to test.
    ctl.hold_changes(400);

    // (1) A re-plumb: a new rate, so a new segment starts and its guard is armed with the window
    // this command asked for.
    let first = CENTER + 600e3;
    let out = controller
        .retune(first, REPLUMB_RATE)
        .expect("the re-plumb itself succeeds: the control plane was answered");
    assert!(
        out.replumbed,
        "this retune must re-plumb (a new rate), or no window guard is armed and the test proves \
         nothing: {out:?}"
    );

    // (2) …and, before the front end has looked at its mailbox, a second command replaces it.
    // Same class and rate, so this is an in-place tune: no new segment, no new guard — and the
    // guard that is running is now waiting for a window nobody is asking for any more.
    let out2 = controller
        .retune(SUPERSEDE_HZ, REPLUMB_RATE)
        .expect("the superseding retune succeeds too");
    assert!(
        !out2.replumbed,
        "the second retune must be an IN-PLACE tune, or it would start a fresh guard and there \
         would be no stale expectation left to strand: {out2:?}"
    );
    let calls = ctl.calls.lock().unwrap().clone();
    assert!(
        calls.iter().any(|c| c == &format!("tune {first}"))
            && calls.iter().any(|c| c == &format!("tune {SUPERSEDE_HZ}")),
        "both commands must have reached the front end for this to be a test about a supersede: \
         {calls:?}"
    );

    // Let the radio look at its mailbox again. It takes ONE change — the coalesced one — and goes
    // to `SUPERSEDE_HZ`. `first` is never applied and will never be reported by any block.
    ctl.hold_changes(0);

    // (3) Capture comes back, inside the settle bound rather than at the end of it.
    let at_retune = captured(&counters);
    let want = at_retune + 100_000;
    let deadline = Instant::now() + RESUME_LIMIT;
    let mut now = at_retune;
    while now < want {
        if Instant::now() > deadline {
            panic!(
                "capture was still silent {RESUME_LIMIT:?} after a window that was superseded \
                 before it arrived: {} samples since the retune, {} blocks dropped for the \
                 window, {} settle timeouts, {} supersedes noticed. THIS IS T-525: the guard is \
                 waiting out `WINDOW_SETTLE_TIMEOUT` for {first} Hz while the radio sits on \
                 {SUPERSEDE_HZ} Hz, and `spectrum/live` answers 410 to everything for the whole \
                 of it.",
                now - at_retune,
                stat(&controller, "blocks_dropped_window"),
                stat(&controller, "window_settle_timeouts"),
                stat(&controller, "window_commands_superseded"),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
        now = captured(&counters);
    }

    // (4) It came back because the guard noticed, not because the bound ran out. These two are
    // the difference between the fix and the pre-existing safety net, and a test that only
    // asserted "capture resumed" would pass on the safety net alone.
    assert_eq!(
        stat(&controller, "window_settle_timeouts"),
        0,
        "capture resumed by serving out the settle bound, not by noticing the supersede: that is \
         T-497's net doing its job over a defect T-525 is supposed to remove"
    );
    assert!(
        stat(&controller, "window_commands_superseded") >= 1,
        "the guard never recorded that its window had been replaced, so whatever resumed capture \
         was not the T-525 rule"
    );

    let st = controller.status();
    assert!(!st.finished, "the run ended: {st:?}");

    handle.stop();
    ctl.finish();
    ctl.run_free();
    if let Ok(summary) = handle.wait() {
        eprintln!("{}", summary.to_text());
    }
}
