//! T-534 — **a `--loop` run keeps looping after a re-plumb and after a capture recovery.**
//!
//! Found by T-474. The reopen policy (the `SourceFactory` `hk serve --replay --loop` passes) was
//! handed to the run's **first** segment only; `replumb()` and `recover()` both started their
//! segment with `reopen: None`. So after any retune that re-plumbed, or any capture recovery, a
//! looping replay simply ended at the recording's end — the user's demo quietly stopped producing
//! data after a retune. The policy now lives on the run, and every segment's capture thread
//! reopens through it.
//!
//! The recording is modelled as a finite *pass* of the scripted receiver
//! (`tests/support/radio.rs`) behind the generic device contract: it ends (`Ok(None)`) after
//! [`PASS`] samples, and the factory opens a fresh pass at the device's default window, exactly as
//! reopening a recording does. That default is **not** where the run was moved, so the test also
//! pins the second half of the fix: a pass reopened on a live run is sent the commanded window,
//! rather than delivering blocks the segment's window guard would drop.
//!
//! **T-1012: every instant this test asserts at is read off the run's own counters, never off
//! "whichever pass happens to be newest when the test thread looks".** Two races lived here:
//!
//! - The recovery's fault was armed on `passes.last()`. When that pass had already delivered its
//!   final block — the capture thread then sits in the lossless gate with `emitted == PASS` until
//!   the consumers drain it, which is most of a pass under load — the pass's next read is its end
//!   (`Ok(None)`), so the budget was never consumed, the reopened pass had none, and the run looped
//!   on for 90 s with `capture_recoveries` at 0 ("capture never recovered"). The stall is now a
//!   fault of the **device** ([`Stall`]), shared by every pass, so whichever pass reads next
//!   consumes it.
//! - The commanded-window assertions read `passes.last()` too, which can be a pass opened a moment
//!   ago whose factory has not sent the rate yet, or whose first block has not been read. They now
//!   read the first pass opened after `retune` returned, and only once it has played out.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_core::{BlockHeader, Source, SourceCapabilities, SourceControl, SourceError};
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineController, PipelineHandle, SourceFactory, SourceInfo,
    TrackInventory, replay_plan,
};
use num_complex::{Complex, Complex32};

const CENTER: f64 = 433.5e6;
const FS: f64 = 500e3;
/// The moved-to rate: another rate, so the retune re-plumbs.
const FS2: f64 = 1e6;
/// Samples in one pass of the "recording".
const PASS: u64 = 200_000;
const BLOCK: usize = 8_192;
const LIMIT: Duration = Duration::from_secs(90);

/// **T-1012: a transient USB stall of the device, not of one pass.** The next `n` reads fail with
/// a device error and the front end then delivers again — `radio::RadioControl::fail_reads_for`'s
/// fault, held here because every pass of the recording is the same device. Armed on a single
/// pass it could land on one that had already delivered its last block, whose next read is its
/// end rather than a read of the device, so the fault was never consumed.
#[derive(Clone, Default)]
struct Stall(Arc<AtomicU64>);

impl Stall {
    fn arm(&self, n: u64) {
        self.0.store(n, Ordering::SeqCst);
    }

    fn read(&self) -> Result<(), SourceError> {
        let consumed = self
            .0
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok();
        if consumed {
            return Err(SourceError::Device {
                source_name: "scripted-radio",
                operation: "receive",
                message: "a USB transfer stalled (scripted, clears)".into(),
            });
        }
        Ok(())
    }
}

/// One pass of the recording: the scripted receiver, ended after [`PASS`] samples.
struct Pass(radio::Radio, Arc<radio::RadioControl>, Stall);

impl Pass {
    fn open(stall: &Stall) -> (Self, Arc<radio::RadioControl>) {
        let (r, c) = radio::Radio::new(CENTER, FS, BLOCK, radio::tone(|_| 50e3));
        c.hold_at(PASS);
        (Self(r, Arc::clone(&c), stall.clone()), c)
    }
}

impl Source for Pass {
    fn capabilities(&self) -> &SourceCapabilities {
        self.0.capabilities()
    }
    fn control(&self) -> Arc<dyn SourceControl> {
        self.0.control()
    }
    fn pausable(&self) -> bool {
        true
    }
    fn read_block(&mut self, s: &mut Vec<Complex32>) -> Result<Option<BlockHeader>, SourceError> {
        self.2.read()?;
        if self.1.emitted() >= PASS {
            return Ok(None);
        }
        self.0.read_block(s)
    }
    fn read_block_ci8(
        &mut self,
        s: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        self.2.read()?;
        if self.1.emitted() >= PASS {
            return Ok(None);
        }
        self.0.read_block_ci8(s)
    }
}

fn stat(controller: &PipelineController, key: &str) -> u64 {
    controller.status().stats[key].as_u64().unwrap_or(0)
}

/// Waits for `loops` to reach `n`, panicking with the run's state if it does not.
fn wait_loops(handle: &PipelineHandle, n: u64, why: &str) {
    let counters = handle.counters();
    let deadline = Instant::now() + LIMIT;
    while counters.source.loops.load(Ordering::Relaxed) < n {
        assert!(
            Instant::now() < deadline,
            "{why}: the replay wrapped {} times, wanted {n}; {:?}",
            counters.source.loops.load(Ordering::Relaxed),
            handle.controller().status()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_looping_replay_keeps_wrapping_after_a_replumb_and_a_recovery() {
    let dir = TempDir::new("t534-loop-replumb");
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(CENTER, FS, t0)).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());

    let stall = Stall::default();
    let (first, first_ctl) = Pass::open(&stall);
    // Every pass's control, newest last, so the test can reach the one capturing now.
    let passes: Arc<Mutex<Vec<Arc<radio::RadioControl>>>> = Arc::new(Mutex::new(vec![first_ctl]));
    let opened = Arc::new(AtomicU64::new(0));
    let reopen: SourceFactory = {
        let (passes, opened, stall) = (Arc::clone(&passes), Arc::clone(&opened), stall.clone());
        Box::new(move || {
            let (p, c) = Pass::open(&stall);
            passes.lock().unwrap().push(c);
            opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(p) as Box<dyn Source>)
        })
    };
    let handle = Pipeline::start(
        cfg,
        Box::new(first),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER,
            start_time: t0,
        },
        Some(reopen),
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let controller = handle.controller();
    let counters = handle.counters();

    // ---- the control: the first segment loops at all ----
    wait_loops(&handle, 2, "the first segment");

    // ---- 1. a re-plumb ----
    let out = controller.retune(CENTER, FS2).expect("retune");
    assert!(out.replumbed, "another rate must re-plumb: {out:?}");
    // The old segment's threads have all ended once `retune` returns (`run::replumb`), so every
    // pass from this index on was opened by the new segment, with the commanded window at FS2.
    let first_after = passes.lock().unwrap().len();
    let at = counters.source.loops.load(Ordering::Relaxed);
    wait_loops(
        &handle,
        at + 2,
        "a re-plumbed segment dropped the run's --loop policy",
    );
    // Two reopens completed after `first_after` was read, and the second opened a pass after the
    // first returned, so the pass at `first_after` exists and its factory call — which sends the
    // commanded window before the capture thread counts the loop — has returned.
    let reopened = Arc::clone(&passes.lock().unwrap()[first_after]);
    let calls = reopened.calls.lock().unwrap().clone();
    assert!(
        calls.iter().any(|c| c == &format!("rate {FS2}")),
        "a pass reopened after the re-plumb was not sent the commanded rate: {calls:?}"
    );
    // Its window is recorded at its first read; wait for it to play out, which it does whichever
    // window it is on (the radio emits either way; only the segment's guard would drop them).
    assert!(
        reopened.wait_emitted(PASS, LIMIT),
        "the first pass reopened after the re-plumb never played out: emitted {}",
        reopened.emitted()
    );
    let windows = reopened.windows.lock().unwrap().clone();
    assert_eq!(
        windows.last().map(|w| w.2),
        Some(FS2),
        "the reopened pass never moved to the commanded window: {windows:?}"
    );

    // ---- 2. a capture recovery ----
    let before = stat(&controller, "capture_recoveries");
    stall.arm(2);
    let deadline = Instant::now() + LIMIT;
    while stat(&controller, "capture_recoveries") <= before {
        assert!(
            Instant::now() < deadline,
            "capture never recovered: {:?}",
            controller.status()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let at = counters.source.loops.load(Ordering::Relaxed);
    wait_loops(
        &handle,
        at + 2,
        "a recovered segment dropped the run's --loop policy",
    );

    handle.stop();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    eprintln!("{}", summary.to_text());
    assert!(!fired, "the run stopped when asked");
    // The scripted stall is reported even though it was absorbed (T-529); nothing else is.
    assert!(
        summary
            .errors
            .iter()
            .all(|e| e.contains("a USB transfer stalled (scripted, clears)")),
        "{:?}",
        summary.errors
    );
    assert!(opened.load(Ordering::SeqCst) >= 6);
}
