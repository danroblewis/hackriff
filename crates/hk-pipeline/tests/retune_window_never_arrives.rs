//! **T-497 — a retune's settle gap is bounded. Capture is never silent for ever.**
//!
//! The user, three times: *"the Retune button kills the live view; `hk serve` stays up but the live
//! capture dies on retune and never recovers."* Two guards were green over it —
//! `a_retune_keeps_connected_stream_consumers_connected` (hk-cli's api_contract, T-417) and the
//! canvas journey's test 2 — and this file is what they were both missing.
//!
//! **Neither of them was asking the wrong question; they were asking it of the wrong radio.** Both
//! drive the mock SDR, and a mock *echoes the window it was told*, so the one state the symptom
//! lives in is unreachable behind it. Measured before any of this was written, on `main`: through
//! the mock the socket survives a retune (2 sockets, 0 closes, 0 errors, rows resuming at the new
//! centre) **and** the rendered live edge keeps advancing — 93–98 % of the pane's freshest strip
//! changing over 8 s, six consecutive retunes in a row, on servers 60 s and 15 min old. There was
//! nothing wrong with either assertion.
//!
//! **What neither of them asserts is that capture resumes AT ALL when the front end does not go
//! where it was sent.** [`hk_pipeline`]'s `WindowGuard` drops every block until the block's own
//! provenance reports the window the re-plumb asked for, to within 1 Hz — and, before this ticket,
//! it waited for that **for ever**. `expect` was cleared only by a matching block. A front end that
//! lands one component away never clears it, so every block is dropped, the ring receives nothing,
//! and the live view stops and stays stopped — while `hk serve` answers normally, `run.finished`
//! is false, `replumbing` is false, and the socket stays connected and silent. That is the user's
//! report, exactly, and it is invisible to every guard that runs on an echoing mock.
//!
//! **The state is reachable without inventing anything.** `hk_core`'s HackRF driver applies a
//! posted control field by field and returns on the first `SourceError` (`apply_change`: sample
//! rate, then baseband filter, then gains, then centre), so a failed baseband-filter write leaves
//! the new *rate* in the provenance and the centre never applied. `replumb`'s own failure path was
//! the second door: it reverted a failed `apply_window` with `let _ =` and then started a segment
//! expecting the window the revert may not have restored. `RadioControl::lose_center` models the
//! first — the front end takes the command and does not move — behind the same generic device
//! contract every other control-plane test uses (CLAUDE.md: e2e drives the SDR device interface).
//!
//! The claim asserted here has three parts, and all three are needed:
//!
//! 1. the front end **was** commanded (the call is in the radio's own log), so this is a test about
//!    a retune that happened, not one that was refused;
//! 2. blocks **are** dropped at first — the settle gap is real and stays real, nothing is admitted
//!    that describes the tuning that ended;
//! 3. and capture **resumes**, on the window the radio is actually on, with the give-up counted on
//!    `window_settle_timeouts` rather than swallowed. A silent recovery would be the same lie in
//!    the other direction.
//!
//! **Non-vacuity.** With [`hk_pipeline::WINDOW_SETTLE_TIMEOUT`]'s give-up removed (the `expect`
//! branch restored to a bare `return self.reject()`), part 3 fails: `source.samples` does not move
//! by a single sample in the 120 s limit and `blocks_dropped_window` climbs the whole time.

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
/// Far enough that it is a real move, close enough to keep the same content class.
const MOVE_HZ: f64 = 400e3;
const RING_S: f64 = 0.5;
const LIMIT: Duration = Duration::from_secs(120);

/// Captured samples that got **past** the window guard into the ring — the one counter that says
/// "the live view has something to draw". `emitted` is the radio's own and moves whether or not a
/// block is admitted, which is precisely the difference this test exists to see.
fn captured(c: &Counters) -> u64 {
    c.source.samples.load(Ordering::Relaxed)
}

fn stat(controller: &hk_pipeline::PipelineController, key: &str) -> u64 {
    controller.status().stats[key].as_u64().unwrap_or(0)
}

#[test]
fn a_retune_whose_window_never_arrives_does_not_silence_capture_for_ever() {
    let dir = TempDir::new("t497-retune-window-never-arrives");
    let (rx, ctl) = radio::Radio::new(CENTER, FS, 16_384, radio::tone(|_| 80e3));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    // The guard under test is only wrapped on a live, window-classed run — the same configuration
    // `hk serve --device …` uses.
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
        ctl.wait_emitted(ctl.emitted() + 500_000, LIMIT),
        "the run never warmed up"
    );
    let warm = captured(&counters);
    assert!(warm > 0, "nothing was captured before the retune");

    // ---- the front end takes the command and does not move ----
    ctl.lose_center(true);
    let target = CENTER + MOVE_HZ;
    let out = controller
        .retune(target, FS * 2.0)
        .expect("the re-plumb itself succeeds: the control plane was answered");
    assert!(
        out.replumbed,
        "this retune must re-plumb (a new rate), or there is no `expect` to get stuck on: {out:?}"
    );
    assert!(
        ctl.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == &format!("tune {target}")),
        "the front end was never told to move, so this is not a test about a retune: {:?}",
        ctl.calls.lock().unwrap()
    );

    // (2) The settle gap is real: blocks describing a window that is not the one asked for are
    // dropped, exactly as before. Nothing here relaxes that.
    let mut dropped = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        dropped = stat(&controller, "blocks_dropped_window");
        if dropped > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        dropped > 0,
        "no block was dropped after a retune the front end did not take — the guard is not \
         guarding, and the rest of this test would prove nothing"
    );

    // (3) …and then capture comes back. THIS is the assertion the two green guards do not make.
    let after_retune = captured(&counters);
    let want = after_retune + 200_000;
    let deadline = Instant::now() + LIMIT;
    let mut now = after_retune;
    while now < want {
        if Instant::now() > deadline {
            panic!(
                "capture never resumed after a retune the front end did not take: {} samples in \
                 {LIMIT:?} (warm {warm}, at the retune {after_retune}), {} blocks dropped for the \
                 window, {} settle timeouts. THIS IS T-497: `hk serve` is alive and answering and \
                 the live capture is gone for ever.",
                now - after_retune,
                stat(&controller, "blocks_dropped_window"),
                stat(&controller, "window_settle_timeouts"),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
        now = captured(&counters);
    }

    // The give-up is a defect signal and is reported as one: "the radio is not where you asked it
    // to be" must never be renderable as if it were.
    assert!(
        stat(&controller, "window_settle_timeouts") >= 1,
        "capture resumed without the guard ever recording that it stopped waiting for the window \
         it asked for — the recovery would then be silent, which is the same dishonesty the other \
         way round"
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
