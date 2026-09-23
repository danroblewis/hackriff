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

/// One pass of the recording: the scripted receiver, ended after [`PASS`] samples.
struct Pass(radio::Radio, Arc<radio::RadioControl>);

impl Pass {
    fn open() -> (Self, Arc<radio::RadioControl>) {
        let (r, c) = radio::Radio::new(CENTER, FS, BLOCK, radio::tone(|_| 50e3));
        c.hold_at(PASS);
        (Self(r, Arc::clone(&c)), c)
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
        if self.1.emitted() >= PASS {
            return Ok(None);
        }
        self.0.read_block(s)
    }
    fn read_block_ci8(
        &mut self,
        s: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
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

    let (first, first_ctl) = Pass::open();
    // Every pass's control, newest last, so the test can reach the one capturing now.
    let passes: Arc<Mutex<Vec<Arc<radio::RadioControl>>>> = Arc::new(Mutex::new(vec![first_ctl]));
    let opened = Arc::new(AtomicU64::new(0));
    let reopen: SourceFactory = {
        let (passes, opened) = (Arc::clone(&passes), Arc::clone(&opened));
        Box::new(move || {
            let (p, c) = Pass::open();
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
    let at = counters.source.loops.load(Ordering::Relaxed);
    wait_loops(
        &handle,
        at + 2,
        "a re-plumbed segment dropped the run's --loop policy",
    );
    // The pass reopened after the re-plumb was sent the commanded window, and delivered at it.
    let last = Arc::clone(passes.lock().unwrap().last().unwrap());
    let calls = last.calls.lock().unwrap().clone();
    assert!(
        calls.iter().any(|c| c == &format!("rate {FS2}")),
        "a pass reopened after the re-plumb was not sent the commanded rate: {calls:?}"
    );
    let windows = last.windows.lock().unwrap().clone();
    assert_eq!(
        windows.last().map(|w| w.2),
        Some(FS2),
        "the reopened pass never moved to the commanded window: {windows:?}"
    );

    // ---- 2. a capture recovery ----
    let before = stat(&controller, "capture_recoveries");
    let current = Arc::clone(passes.lock().unwrap().last().unwrap());
    current.fail_reads_for(2);
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
