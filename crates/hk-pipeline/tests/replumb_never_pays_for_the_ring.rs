//! T-536 — **the IQ capture ring's allocation is paid for by the thread that allocates it**, and
//! never by a thread on the capture path.
//!
//! Reserving disk blocks is cheap and *deferred*: `F_PREALLOCATE` (macOS) and `fallocate` (Linux)
//! return long before the filesystem has committed the extents they promised, and the bill lands
//! on the **first fsync of that file** — whoever does it, whenever, on whatever thread. The IQ
//! capture ring is opened on its own background thread (`hk-iqbuffer-alloc`, T-178) precisely so a
//! slow open never holds up the run, but that thread used to do only the cheap half. The expensive
//! half was left for the next fsync of the ring, which is
//! [`hk_store::iqbuffer::IqBufferWriter::checkpoint`], on the feeder thread `hk-iqbuffer` — and a
//! feeder's *last act before its segment ends is a checkpoint*. So **a routine re-plumb paid for
//! the whole ring with capture off**: measured on this Mac's APFS, a re-plumb whose always-on
//! readers stalled 55 ms with the buffer off stalled **3.0 s** with a 12 GB ring, and the
//! straggler report of T-531 named `hk-capture`, `hk-detect` and the rest while it happened.
//!
//! The filesystem here models exactly that: [`DeferredAllocation::preallocate`] returns at once
//! and leaves a **debt**, and whichever fsync comes next pays it. Two things are asserted, and
//! the first is the one that goes red:
//!
//! 1. **Who paid.** The debt must be settled on `hk-iqbuffer-alloc`. Without the fix it is settled
//!    on `hk-iqbuffer`, the segment's ring reader — the thread whose delay is a capture delay.
//! 2. **The bound.** A re-plumb with that ring open must not stall the always-on readers beyond
//!    [`STALL_BOUND`], which is a third of the debt. This is measured where a user would see it:
//!    the samples the **device** is asked for, through the generic receiver contract, not the
//!    pipeline's own bookkeeping.
//!
//! The debt is one-shot because a filesystem's is: there is one set of uncommitted extents and one
//! fsync commits them. So if a machine is slow enough that the feeder's periodic checkpoint
//! (`CHECKPOINT_INTERVAL`, 1 s) beats the re-plumb to it, assertion 2 passes trivially — but
//! assertion 1 has already failed, and it cannot be timing-dependent: `sync_allocation` is either
//! called from the allocation or it is not.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::iqbuffer::{FsSpace, IqBufferHooks};
use serde_json::json;

const CENTER: f64 = 433.92e6;
const FS: f64 = 500e3;
const RING_S: f64 = 0.5;
const LIMIT: Duration = Duration::from_secs(120);

/// What a large ring's deferred allocation costs the first fsync that meets it. Real values
/// measured on APFS: 2.0 s for the default 4.8 GB quota, 7.6 s at 12 GB. Three seconds is inside
/// that range and far outside anything scheduling noise produces.
const DEBT: Duration = Duration::from_secs(3);

/// How long a re-plumb may stall the always-on readers. A third of [`DEBT`], so the assertion can
/// only be satisfied by the debt being paid somewhere else — never by a lucky margin. The
/// re-plumb's own floor is tens of milliseconds (`join_workers` polls at 50 ms), so there is an
/// order of magnitude of headroom for a loaded machine.
const STALL_BOUND: Duration = Duration::from_secs(1);

/// A filesystem whose block reservations are **deferred**: `preallocate` returns at once and the
/// next fsync of the ring pays for it. This is what APFS (and ext4) actually do; it is the whole
/// reason the cost moved off the thread that caused it.
struct DeferredAllocation {
    debt: Mutex<Option<Duration>>,
    /// The name of the thread that settled the debt, and how long it took.
    paid: Mutex<Option<(String, Duration)>>,
}

impl DeferredAllocation {
    fn new() -> Self {
        Self {
            debt: Mutex::new(None),
            paid: Mutex::new(None),
        }
    }

    /// Settles whatever the outstanding reservations cost, on the calling thread.
    fn settle(&self) {
        let Some(owed) = self.debt.lock().unwrap().take() else {
            return;
        };
        let who = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_owned();
        let t = Instant::now();
        std::thread::sleep(owed);
        *self.paid.lock().unwrap() = Some((who, t.elapsed()));
    }

    fn payer(&self) -> Option<String> {
        self.paid.lock().unwrap().as_ref().map(|(w, _)| w.clone())
    }
}

impl IqBufferHooks for DeferredAllocation {
    fn fs_space(&self, _: &Path) -> io::Result<FsSpace> {
        Ok(FsSpace {
            free: 2 << 40,
            total: 4 << 40,
        })
    }

    /// Reserves the blocks without committing them: instant, and a debt is owed.
    fn preallocate(&self, file: &File, len: u64) -> io::Result<bool> {
        file.set_len(len)?;
        *self.debt.lock().unwrap() = Some(DEBT);
        Ok(true)
    }

    fn sync_allocation(&self, _: &File) -> io::Result<()> {
        self.settle();
        Ok(())
    }

    fn before_sync(&self) -> io::Result<()> {
        self.settle();
        Ok(())
    }
}

/// Watches the samples the **device** is asked for and keeps the longest interval in which that
/// number did not move: the always-on readers' stall, seen from outside the pipeline.
struct CaptureWatch {
    stop: Arc<AtomicBool>,
    worst_ns: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CaptureWatch {
    fn start(ctl: Arc<radio::RadioControl>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worst_ns = Arc::new(AtomicU64::new(0));
        let (s, w) = (Arc::clone(&stop), Arc::clone(&worst_ns));
        let handle = std::thread::spawn(move || {
            let (mut last_n, mut last_t) = (ctl.emitted(), Instant::now());
            while !s.load(Ordering::Relaxed) {
                let n = ctl.emitted();
                let now = Instant::now();
                if n != last_n {
                    w.fetch_max(
                        now.duration_since(last_t).as_nanos() as u64,
                        Ordering::Relaxed,
                    );
                    (last_n, last_t) = (n, now);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            stop,
            worst_ns,
            handle: Some(handle),
        }
    }

    fn worst(mut self) -> Duration {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
        Duration::from_nanos(self.worst_ns.load(Ordering::Relaxed))
    }
}

#[test]
fn a_re_plumb_never_pays_for_the_iq_ring() {
    let dir = TempDir::new("replumb-never-pays-for-the-ring");
    let fsys = Arc::new(DeferredAllocation::new());
    let (rx, ctl) = radio::Radio::new(CENTER, FS, 16_384, radio::tone(|_| 80.0e3));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": RING_S } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    cfg.iq_buffer.enabled = Some(true);
    // 800 MB: enough to be a ring worth pre-allocating, small enough to be one
    // `ALLOCATION_STEP_BYTES` step, so the injected debt is owed exactly once. Nothing here is
    // really written — `DeferredAllocation` makes a sparse file — and the size that matters is
    // `DEBT`, not this.
    cfg.iq_buffer.retention_s = 20.0;
    cfg.iq_buffer.max_rate_hz = Some(20.0e6);
    cfg.iq_buffer_hooks = Some(Arc::clone(&fsys) as Arc<dyn IqBufferHooks>);
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

    // 1. The open finishes, and the allocation's bill is settled by the thread that allocated.
    assert!(
        handle.iq_buffer().wait_allocated(LIMIT),
        "the IQ capture ring never finished opening"
    );
    assert_eq!(
        fsys.payer().as_deref(),
        Some("hk-iqbuffer-alloc"),
        "the ring's deferred allocation was not paid for by the thread that opened it, so the \
         bill is still outstanding for whichever capture-path thread next fsyncs the ring"
    );

    // 2. With that ring open, a re-plumb does not stall the always-on readers.
    let warm = ctl.emitted() + 200_000;
    assert!(ctl.wait_emitted(warm, LIMIT), "the run never warmed up");
    let watch = CaptureWatch::start(Arc::clone(&ctl));
    let before = ctl.emitted();
    let out = controller
        .retune(CENTER, FS * 2.0)
        .expect("a rate change is a legal retune");
    assert!(out.replumbed, "the retune did not re-plumb the run");
    assert!(
        ctl.wait_emitted(before + 100_000, LIMIT),
        "capture never resumed after the re-plumb"
    );
    let stall = watch.worst();
    assert!(
        stall < STALL_BOUND,
        "a re-plumb stalled the always-on readers for {stall:.1?} (bound {STALL_BOUND:?}); the \
         ring's allocation is being paid for on the capture path"
    );

    ctl.finish();
    let (_summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
}
