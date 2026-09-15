//! T-178: the IQ capture ring survives a restart, end to end **through the mock SDR device
//! interface** (a real-time tone recording behind `MockSdrDriver`), never feeding files into the
//! pipeline.
//!
//! - Run 1 buffers, exports a clip by stream index, and stops; run 2 opens the same data
//!   directory. Its status lists run 1's segments unchanged (ids, stream indices, sample counts,
//!   sample-clock span, tuning), reports them as recovered, and a clip of the same range of run 1
//!   is byte-identical to the source recording and to the clip exported before the restart.
//! - The ring's files and size are the same in both runs (allocated up front, never grown).
//! - Run 2 replays the recording from its start, so its sample clock overlaps run 1: a time range
//!   without `run` is a conflict, and an index range without `run` selects run 2.

mod common;

use std::time::{Duration, Instant};

use common::{TempDir, tone_recording, wait_guarded};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Pacing, Source};
use hk_pipeline::class::window_class;
use hk_pipeline::iqbuffer::{ClipFailure, ClipRange, ClipRequest};
use hk_pipeline::{
    Pipeline, PipelineConfig, PipelineHandle, SourceInfo, TrackInventory, replay_plan,
};
use hk_store::iqbuffer::{Allocation, IqBufferConfig, IqBufferStatus};

const FS: f64 = 1.0e6;
const CENTER_HZ: f64 = 100.0e6;
const LIMIT: Duration = Duration::from_secs(120);
const RING_BYTES: u64 = 32 << 20;

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
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The recording's ci8 bytes of stream indices `[g, g + n)` (the mock loops it).
fn source_bytes(recorded: &[u8], g: u64, n: u64) -> Vec<u8> {
    let len = recorded.len() as u64 / 2;
    (g..g + n)
        .flat_map(|i| {
            let j = (2 * (i % len)) as usize;
            [recorded[j], recorded[j + 1]]
        })
        .collect()
}

/// A real-time mock run over `meta` in `dir` with a small ring.
fn start_run(dir: &std::path::Path, meta: &std::path::Path) -> (PipelineHandle, SourceInfo) {
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
    cfg.iq_buffer = IqBufferConfig {
        enabled: Some(true),
        retention_s: 600.0,
        max_bytes: Some(RING_BYTES),
        min_free_bytes: Some(0),
        max_clip_bytes: 8 << 20,
        ..IqBufferConfig::default()
    };
    let handle = Pipeline::start(
        cfg,
        Box::new(source),
        info,
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    (handle, info)
}

/// (id, global_index, samples, t0_ns, t1_ns, centre, rate) of a run's listed segments.
fn keys(s: &IqBufferStatus, run: u64) -> Vec<(u64, u64, u64, i64, i64, f64, f64)> {
    s.segments
        .iter()
        .filter(|g| g.run == run)
        .map(|g| {
            (
                g.id,
                g.global_index,
                g.samples,
                g.t0_ns,
                g.t1_ns,
                g.center_hz,
                g.sample_rate_hz,
            )
        })
        .collect()
}

/// The ring's files (name, size).
fn ring_files(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut v: Vec<_> = std::fs::read_dir(dir.join(hk_store::iqbuffer::DIR_NAME))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != hk_store::iqbuffer::JOURNAL_FILE)
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                e.metadata().unwrap().len(),
            )
        })
        .collect();
    v.sort();
    v
}

fn stop(handle: PipelineHandle) {
    handle.stop();
    let (summary, fired) = wait_guarded(handle, LIMIT);
    assert!(!fired, "the run stopped");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
}

#[test]
fn capture_ring_survives_a_restart_with_byte_identical_clips() {
    let dir = TempDir::new("t178-ring-restart");
    let meta = tone_recording(&dir.0.join("rec"), "tone", FS, 4.0, CENTER_HZ, None);
    let recorded = std::fs::read(meta.with_extension("sigmf-data")).unwrap();
    let range = ClipRange::Index {
        start: 100_000,
        end: 400_000,
    };

    // --- Run 1 -------------------------------------------------------------------------------------
    let (handle, info) = start_run(&dir.0, &meta);
    let buffer = handle.iq_buffer();
    assert!(buffer.enabled());
    let status = || buffer.status(None, None, 1000);
    let s = wait_status("0.5 s buffered", status, |s| s.samples >= 500_000);
    let run1 = s.run.expect("a run");
    assert_eq!((s.recovered_segments, s.persisted), (0, true), "{s:?}");
    assert_eq!(s.allocation, Some(Allocation::Full));
    assert_eq!(s.allocated_bytes, s.slot_count * s.chunk_bytes);
    assert_eq!(s.segments[0].t0_ns, info.start_time.as_unix_nanos());
    let before = buffer
        .export_clip(&ClipRequest {
            range,
            band: None,
            label: Some("before the restart".into()),
            run: None,
        })
        .unwrap();
    let before_data = std::fs::read(&before.data_path).unwrap();
    assert!(
        before_data == source_bytes(&recorded, 100_000, 300_000),
        "run 1's clip is the source's bytes"
    );
    let files1 = ring_files(&dir.0);
    stop(handle);
    let s1 = buffer.status(None, None, 1000);
    drop(buffer);
    assert!(!keys(&s1, run1).is_empty(), "{s1:?}");

    // --- Run 2 on the same data directory ------------------------------------------------------------
    let (handle, _) = start_run(&dir.0, &meta);
    let buffer = handle.iq_buffer();
    assert!(
        buffer.enabled(),
        "{:?}",
        buffer.status(None, None, 0).reason
    );
    let status = || buffer.status(None, None, 1000);
    let s2 = status();
    assert_eq!(s2.run, Some(run1 + 1), "{s2:?}");
    assert_eq!(s2.recovered_segments, s1.segments_total, "{s2:?}");
    assert_eq!(
        keys(&s2, run1),
        keys(&s1, run1),
        "run 1's segments as they were"
    );
    assert_eq!(ring_files(&dir.0), files1, "the same ring files and sizes");
    let after = buffer
        .export_clip(&ClipRequest {
            range,
            band: None,
            label: Some("after the restart".into()),
            run: Some(run1),
        })
        .unwrap();
    let after_data = std::fs::read(&after.data_path).unwrap();
    assert_eq!(after.samples, 300_000);
    assert!(
        after_data == before_data,
        "the clip of run 1 after the restart is byte-identical"
    );
    assert!(after_data == source_bytes(&recorded, 100_000, 300_000));
    assert_eq!(after.captures[0].run, run1);
    eprintln!(
        "restart: run {run1} -> {}, {} segments recovered, clip of {} samples identical",
        run1 + 1,
        s2.recovered_segments,
        after.samples
    );

    // Run 2 replays from the recording's start: its times overlap run 1.
    let s = wait_status("0.5 s buffered in run 2", status, |s| {
        s.segments
            .iter()
            .filter(|g| g.run == run1 + 1)
            .map(|g| g.samples)
            .sum::<u64>()
            >= 500_000
    });
    let t0 = s.segments[0].t0_ns;
    let err = buffer
        .export_clip(&ClipRequest {
            range: ClipRange::Time {
                t0_ns: t0 + 100_000_000,
                t1_ns: t0 + 400_000_000,
            },
            band: None,
            label: None,
            run: None,
        })
        .unwrap_err();
    assert!(matches!(err, ClipFailure::Conflict(_)), "{err:?}");
    let now = buffer
        .export_clip(&ClipRequest {
            range,
            band: None,
            label: None,
            run: None,
        })
        .unwrap();
    assert_eq!(
        now.captures[0].run,
        run1 + 1,
        "an index range selects this run"
    );
    assert_eq!(ring_files(&dir.0), files1);
    stop(handle);
}
