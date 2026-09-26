//! T-536: **the IQ capture ring's allocation is paid for by the thread that allocates it**, and
//! never by a thread on the capture path.
//!
//! Reserving disk blocks is cheap and *deferred*: `F_PREALLOCATE` (macOS) and `fallocate` (Linux)
//! return long before the filesystem has committed the extents they promised, and the bill lands
//! on the **first fsync of that file**, whoever does it, whenever, on whatever thread. The IQ
//! capture ring is opened on its own background thread (`hk-iqbuffer-alloc`, T-178) precisely so a
//! slow open never holds up the run, but that thread used to do only the cheap half. The expensive
//! half was left for the next fsync of the ring, which is
//! [`hk_store::iqbuffer::IqBufferWriter::checkpoint`], on the feeder thread `hk-iqbuffer`, and a
//! feeder's *last act before its segment ends is a checkpoint*. So **a routine re-plumb paid for
//! the whole ring with capture off**: measured on this Mac's APFS, a re-plumb whose always-on
//! readers stalled 55 ms with the buffer off stalled **3.0 s** with a 12 GB ring, and the
//! straggler report of T-531 named `hk-capture`, `hk-detect` and the rest while it happened.
//!
//! The filesystem here models exactly that: [`DeferredAllocation::preallocate`] returns at once
//! and leaves a **debt**, and whichever fsync comes next pays it.
//!
//! # Two tests: what the gate proves, and what only a quiet box can measure (T-973)
//!
//! [`a_re_plumb_never_pays_for_the_iq_ring`] is the **gate** half, and nothing in it is a clock
//! bound. It proves, from state:
//!
//! 1. **Who paid.** The debt is settled on `hk-iqbuffer-alloc`. Without T-536's fix it is settled
//!    on `hk-iqbuffer`, the segment's ring reader, the thread whose delay is a capture delay;
//!    with the allocator never syncing at all, the ring opens without ever reaching
//!    `sync_allocation`, and that goes red at once.
//! 2. **Nothing on the re-plumb path waits for the allocation.** The allocator is *parked* inside
//!    `sync_allocation` (the debt outstanding) while the run captures and re-plumbs. The re-plumb
//!    must complete and capture must resume with the allocation still parked, and no thread may
//!    have paid the debt meanwhile. A re-plumb that joined the allocator, or a feeder that
//!    fsynced the half-open ring on its way out, cannot pass that: the first never resumes, the
//!    second becomes the payer.
//!
//! [`a_re_plumb_with_the_ring_open_stalls_the_readers_under_a_second`] is the **latency** half:
//! with the ring open, a re-plumb must not stall the always-on readers beyond [`STALL_BOUND`],
//! measured where a user would see it: the samples the **device** is asked for, through the
//! generic receiver contract. It lives in the `timing` tier (`.config/nextest.toml`,
//! `just timing`, docs/10 §3.6) because its assertion *is* a wall-clock bound, and on the gate's
//! box that bound measures the disk more than the code. T-973 measured where a re-plumb's stall
//! goes, with T-536's fix working (the debt paid on `hk-iqbuffer-alloc` in every run): **0.2 to
//! 0.9 s at load ~25**, 1.8 s and 13.1 s seen during gates. Almost all of it is `hk-history`
//! ending its segment: the scheme-1 `checkpoint` (30 to 200 ms), the view writer waiting for its
//! producer and then joining `hk-view-io` while that thread writes and fsyncs tiles (100 to
//! 300 ms). All of that is disk I/O with other processes writing to the same volume. The ring's
//! share was zero. So the gate half above is the regression guard for T-536, and this bound is
//! for a quiet box, as before, with the value unchanged.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
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

/// How long a re-plumb may stall the always-on readers (the `timing` tier's bound). A third of
/// [`DEBT`], so the assertion can only be satisfied by the debt being paid somewhere else, never
/// by a lucky margin. The rest of a re-plumb is not free: T-973 measured 0.2 to 0.9 s of it on a
/// loaded box, nearly all `hk-history`'s segment-end tile writes (see the module docs), which is
/// why this bound is for a quiet box and the gate asserts T-536 from state instead.
const STALL_BOUND: Duration = Duration::from_secs(1);

/// A filesystem whose block reservations are **deferred**: `preallocate` returns at once and the
/// next fsync of the ring pays for it. This is what APFS (and ext4) actually do; it is the whole
/// reason the cost moved off the thread that caused it.
///
/// Optionally **parked**: the allocator's `sync_allocation` then waits for [`Self::release`]
/// before it pays, so a test can act while the ring is still opening with the debt outstanding.
struct DeferredAllocation {
    debt: Mutex<Option<Duration>>,
    /// The name of the thread that settled the debt, and how long it took.
    paid: Mutex<Option<(String, Duration)>>,
    park: bool,
    /// `(the allocator is parked in sync_allocation, it has been released)`.
    gate: Mutex<(bool, bool)>,
    moved: Condvar,
}

impl DeferredAllocation {
    fn new(park: bool) -> Self {
        Self {
            debt: Mutex::new(None),
            paid: Mutex::new(None),
            park,
            gate: Mutex::new((false, false)),
            moved: Condvar::new(),
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

    fn parked(&self) -> bool {
        self.gate.lock().unwrap().0
    }

    /// Lets a parked allocator go on to pay its debt.
    fn release(&self) {
        self.gate.lock().unwrap().1 = true;
        self.moved.notify_all();
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
        if self.park {
            let mut g = self.gate.lock().unwrap();
            g.0 = true;
            self.moved.notify_all();
            while !g.1 {
                g = self.moved.wait(g).unwrap();
            }
        }
        self.settle();
        Ok(())
    }

    fn before_sync(&self) -> io::Result<()> {
        self.settle();
        Ok(())
    }
}

/// Releases a parked allocator when dropped. Declared **after** the pipeline handle, so it drops
/// first: a failed assertion must not leave the handle's drop joining an allocator that is
/// waiting for a release that never comes.
struct ReleaseOnDrop(Arc<DeferredAllocation>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
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

/// A live, lossless run with the IQ ring on, its filesystem `fsys`.
fn start(
    dir: &TempDir,
    fsys: &Arc<DeferredAllocation>,
) -> (PipelineHandle, Arc<radio::RadioControl>) {
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
    // really written (`DeferredAllocation` makes a sparse file), and the size that matters is
    // `DEBT`, not this.
    cfg.iq_buffer.retention_s = 20.0;
    cfg.iq_buffer.max_rate_hz = Some(20.0e6);
    cfg.iq_buffer_hooks = Some(Arc::clone(fsys) as Arc<dyn IqBufferHooks>);
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
    (handle, ctl)
}

/// Re-plumbs the run (a rate change) and waits for the device to be read again; the samples the
/// device had delivered when the retune was asked for.
fn replumb(handle: &PipelineHandle, ctl: &radio::RadioControl, fs: f64, why: &str) -> u64 {
    let before = ctl.emitted();
    let out = handle
        .controller()
        .retune(CENTER, fs)
        .expect("a rate change is a legal retune");
    assert!(out.replumbed, "the retune did not re-plumb the run");
    assert!(ctl.wait_emitted(before + 100_000, LIMIT), "{why}");
    before
}

/// The gate half: T-536 asserted from state, with no clock bound (see the module docs).
#[test]
fn a_re_plumb_never_pays_for_the_iq_ring() {
    let dir = TempDir::new("replumb-never-pays-for-the-ring");
    let fsys = Arc::new(DeferredAllocation::new(true));
    let (handle, ctl) = start(&dir, &fsys);
    let _release = ReleaseOnDrop(Arc::clone(&fsys));

    // 1. The ring's open reaches `sync_allocation` (and parks there). An open that finishes
    //    without it has left its deferred allocation for a capture-path fsync to pay.
    let deadline = Instant::now() + LIMIT;
    while !fsys.parked() && Instant::now() < deadline {
        if handle.iq_buffer().wait_allocated(Duration::from_millis(10)) {
            break;
        }
    }
    assert!(
        fsys.parked(),
        "the IQ capture ring's open never synced the allocation it made, so its deferred cost \
         is left for whichever capture-path thread next fsyncs the ring"
    );

    // 2. With the allocation still outstanding, the run captures, re-plumbs and captures again:
    //    nothing on the re-plumb path waits for the allocator, and nothing pays its debt.
    let warm = ctl.emitted() + 200_000;
    assert!(
        ctl.wait_emitted(warm, LIMIT),
        "the run never warmed up while its IQ ring was still allocating"
    );
    replumb(
        &handle,
        &ctl,
        FS * 2.0,
        "capture never resumed after a re-plumb made while the IQ ring was still allocating: \
         the re-plumb is waiting on the ring's allocation",
    );
    assert!(
        fsys.parked() && !handle.iq_buffer().enabled(),
        "the allocator left `sync_allocation` before it was released, so the re-plumb above was \
         not made with the allocation outstanding"
    );
    assert_eq!(
        fsys.payer(),
        None,
        "a thread other than the allocator paid the ring's deferred allocation during a re-plumb"
    );

    // 3. Released, the allocator pays its own bill, and the ring opens.
    fsys.release();
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
    assert!(
        handle.iq_buffer().enabled(),
        "the IQ capture ring did not open"
    );

    // 4. With the ring open (its feeder now checkpoints it at every segment end), a re-plumb
    //    still completes.
    replumb(
        &handle,
        &ctl,
        FS,
        "capture never resumed after a re-plumb with the IQ ring open",
    );

    ctl.finish();
    let (_summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
}

/// The `timing` tier's half: the stall a re-plumb costs the always-on readers with the ring open,
/// against [`STALL_BOUND`]. Out of the gate (docs/10 §3.6, `just timing`); see the module docs for
/// what T-973 measured the stall to be made of.
#[test]
fn a_re_plumb_with_the_ring_open_stalls_the_readers_under_a_second() {
    let dir = TempDir::new("replumb-ring-open-stall");
    let fsys = Arc::new(DeferredAllocation::new(false));
    let (handle, ctl) = start(&dir, &fsys);

    assert!(
        handle.iq_buffer().wait_allocated(LIMIT),
        "the IQ capture ring never finished opening"
    );
    assert_eq!(
        fsys.payer().as_deref(),
        Some("hk-iqbuffer-alloc"),
        "the ring's deferred allocation was not paid for by the thread that opened it"
    );

    let warm = ctl.emitted() + 200_000;
    assert!(ctl.wait_emitted(warm, LIMIT), "the run never warmed up");
    let watch = CaptureWatch::start(Arc::clone(&ctl));
    replumb(
        &handle,
        &ctl,
        FS * 2.0,
        "capture never resumed after the re-plumb",
    );
    let stall = watch.worst();
    assert!(
        stall < STALL_BOUND,
        "a re-plumb stalled the always-on readers for {stall:.1?} (bound {STALL_BOUND:?})"
    );

    ctl.finish();
    let (_summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run had to be stopped by the watchdog");
}
