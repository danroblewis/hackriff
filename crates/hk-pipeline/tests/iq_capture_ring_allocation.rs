//! T-178 fix round: the IQ capture ring opens **in the background**, through the mock SDR device
//! interface.
//!
//! - A 1 h retention at 20 Msps (a 144 GB quota, on an injected filesystem that makes a sparse
//!   file and holds allocation until the test releases it) never holds up the run's start: the
//!   pipeline starts and the buffer answers `allocation: "allocating"` with a progress fraction at
//!   once, clips are unavailable, and buffering starts once allocation completes.
//! - A ring whose lock another holder has reports `allocation: "locked"` with a reason instead of
//!   failing the run.

mod common;

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use common::{TempDir, tone_recording, wait_guarded};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Pacing, Source};
use hk_pipeline::class::window_class;
use hk_pipeline::iqbuffer::{ClipFailure, ClipRange, ClipRequest};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use hk_store::iqbuffer::{
    Allocation, FsSpace, IqBuffer, IqBufferConfig, IqBufferHooks, IqBufferStatus, MAX_CHUNK_BYTES,
};

const FS: f64 = 1.0e6;
const CENTER_HZ: f64 = 100.0e6;
const LIMIT: Duration = Duration::from_secs(60);

/// A 2 TiB filesystem whose ring allocation makes a sparse file and, after its first step, waits
/// for the test to release it.
#[derive(Default)]
struct GatedAllocation {
    released: Mutex<bool>,
    cv: Condvar,
    steps: Mutex<u64>,
}

impl GatedAllocation {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

impl IqBufferHooks for GatedAllocation {
    fn fs_space(&self, _: &Path) -> io::Result<FsSpace> {
        Ok(FsSpace {
            free: 2 << 40,
            total: 4 << 40,
        })
    }

    fn preallocate(&self, file: &File, len: u64) -> io::Result<bool> {
        let step = {
            let mut s = self.steps.lock().unwrap();
            *s += 1;
            *s
        };
        if step > 1 {
            let mut r = self.released.lock().unwrap();
            while !*r {
                r = self.cv.wait(r).unwrap();
            }
        }
        file.set_len(len)?;
        Ok(false)
    }
}

fn start_run(
    dir: &Path,
    meta: &Path,
    iq: IqBufferConfig,
    hooks: Option<Arc<dyn IqBufferHooks>>,
) -> PipelineHandle {
    let driver = MockSdrDriver::new(
        meta,
        MockOptions {
            end: MockEnd::Loop,
            block_len: 16_384,
            pacing: Pacing::RealTime { speed: 1.0 },
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let info = SourceInfo {
        sample_rate_hz: source.recording().sample_rate_hz,
        center_hz: source.recording().center_hz,
        start_time: source.start_time(),
    };
    let mut cfg = PipelineConfig::new(
        dir,
        replay_plan(info.center_hz, info.sample_rate_hz, info.start_time),
    )
    .unwrap();
    cfg.source_class = window_class(CENTER_HZ, FS);
    cfg.live_window_class = true;
    cfg.lossless = source.pausable();
    cfg.settings.chains = Some(Vec::new());
    cfg.iq_buffer = iq;
    cfg.iq_buffer_hooks = hooks;
    Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap()
}

fn wait_status(
    what: &str,
    status: impl Fn() -> IqBufferStatus,
    f: impl Fn(&IqBufferStatus) -> bool,
) -> IqBufferStatus {
    let deadline = Instant::now() + LIMIT;
    loop {
        let s = status();
        if f(&s) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {s:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stop(handle: PipelineHandle) {
    handle.stop();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run stopped");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}

fn clip() -> ClipRequest {
    ClipRequest {
        range: ClipRange::Index {
            start: 0,
            end: 1000,
        },
        band: None,
        label: None,
        run: None,
    }
}

#[test]
fn a_144_gb_ring_allocates_in_the_background_while_the_run_answers() {
    let dir = TempDir::new("t178-alloc-bg");
    let meta = tone_recording(&dir.0.join("rec"), "tone", FS, 2.0, CENTER_HZ, None);
    let gate = Arc::new(GatedAllocation::default());
    let hour = IqBufferConfig {
        enabled: Some(true),
        retention_s: 3600.0,
        max_bytes: None,
        min_free_bytes: Some(0),
        ..IqBufferConfig::default()
    };
    assert_eq!(hour.quota_bytes(), 144_000_000_000);
    let t = Instant::now();
    let handle = start_run(&dir.0, &meta, hour, Some(gate.clone()));
    let started = t.elapsed();
    let buffer = handle.iq_buffer();
    // Allocation is held by the gate, yet the run started and the buffer answers at once.
    let t = Instant::now();
    let s = buffer.status(None, None, 1000);
    assert!(
        t.elapsed() < Duration::from_millis(200),
        "{:?}",
        t.elapsed()
    );
    assert!(started < Duration::from_secs(10), "start took {started:?}");
    assert_eq!(s.allocation, Some(Allocation::Allocating), "{s:?}");
    assert!(!s.enabled && buffer.active() && !buffer.enabled(), "{s:?}");
    assert!(
        s.reason
            .as_deref()
            .is_some_and(|r| r.contains("allocating")),
        "{s:?}"
    );
    assert_eq!(s.quota_bytes, 144_000_000_000);
    let s = wait_status(
        "the first allocation step",
        || buffer.status(None, None, 0),
        |s| s.allocation_progress.is_some_and(|p| p > 0.0),
    );
    let p = s.allocation_progress.unwrap();
    assert!(
        p < 0.05 && s.allocation == Some(Allocation::Allocating),
        "{s:?}"
    );
    assert!(matches!(
        buffer.export_clip(&clip()),
        Err(ClipFailure::Unavailable(_))
    ));
    // Capture keeps running while the ring allocates.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        buffer.status(None, None, 0).samples,
        0,
        "nothing buffered yet"
    );
    assert!(!buffer.wait_allocated(Duration::from_millis(50)));

    gate.release();
    assert!(buffer.wait_allocated(LIMIT));
    let s = buffer.status(None, None, 0);
    let slots = 144_000_000_000 / MAX_CHUNK_BYTES;
    assert_eq!(
        (
            s.enabled,
            s.allocation,
            s.allocation_progress,
            s.slot_count,
            s.allocated_bytes,
            s.preallocated
        ),
        (
            true,
            Some(Allocation::Full),
            Some(1.0),
            slots,
            slots * MAX_CHUNK_BYTES,
            false
        ),
        "{s:?}"
    );
    wait_status(
        "buffering after allocation",
        || buffer.status(None, None, 0),
        |s| s.samples >= 100_000,
    );
    if let Err(e) = buffer.export_clip(&clip()) {
        // The first buffered index may be after the allocation, not 0.
        assert!(matches!(e, ClipFailure::NotFound(_)), "{e:?}");
    }
    stop(handle);
    drop(buffer);
    let _ = std::fs::remove_dir_all(&dir.0);
}

#[test]
fn a_ring_locked_by_another_holder_reports_locked_and_the_run_carries_on() {
    let dir = TempDir::new("t178-alloc-locked");
    let meta = tone_recording(&dir.0.join("rec"), "tone", FS, 2.0, CENTER_HZ, None);
    let small = IqBufferConfig {
        enabled: Some(true),
        retention_s: 600.0,
        max_bytes: Some(8 << 20),
        min_free_bytes: Some(0),
        ..IqBufferConfig::default()
    };
    let (held, held_writer) =
        IqBuffer::open(&dir.0.join(hk_store::iqbuffer::DIR_NAME), small).unwrap();
    let handle = start_run(&dir.0, &meta, small, None);
    let buffer = handle.iq_buffer();
    assert!(buffer.wait_allocated(LIMIT));
    let s = buffer.status(None, None, 0);
    assert_eq!(
        (s.enabled, s.allocation, s.allocation_progress),
        (false, Some(Allocation::Locked), None),
        "{s:?}"
    );
    assert!(
        s.reason
            .as_deref()
            .is_some_and(|r| r.contains("in use by another process")),
        "{s:?}"
    );
    assert!(s.dir.is_some(), "{s:?}");
    let e = buffer.export_clip(&clip()).unwrap_err();
    assert!(
        matches!(&e, ClipFailure::Unavailable(m) if m.contains("in use by another process")),
        "{e:?}"
    );
    stop(handle);
    drop(buffer);
    drop(held_writer);
    drop(held);
}
