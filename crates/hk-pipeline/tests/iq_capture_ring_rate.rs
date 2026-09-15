//! T-178 fix round: **measurement** (ignored by default, run with
//! `cargo nextest run -p hk-pipeline --run-ignored ignored-only -E 'binary(iq_capture_ring_rate)' --no-capture`).
//!
//! The mock SDR device at 20 Msps, paced in real time, runs `HK_MEASURE_S` seconds (default 30)
//! without and then with the IQ capture ring on the real filesystem (1 GiB cap, so it wraps
//! and checkpoints every slot and every second). It prints, per run, what the source produced,
//! what the always-on readers lost, and the buffer's stored, dropped (reader lapped by the
//! capture ring), failed and fsync-failed samples.

mod common;

use std::time::{Duration, Instant};

use common::{TempDir, tone_recording, wait_guarded};
use hk_core::{MockEnd, MockOptions, MockSdrDriver, Pacing, Source};
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_store::iqbuffer::IqBufferConfig;

const FS: f64 = 20.0e6;
const CENTER_HZ: f64 = 100.0e6;

#[test]
#[ignore = "measurement: 2 × 30 s at 20 Msps on the real disk"]
fn iq_ring_drops_at_20_msps_real_time() {
    let secs: f64 = std::env::var("HK_MEASURE_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30.0);
    let dir = TempDir::new("t178-rate");
    let meta = tone_recording(&dir.0.join("rec"), "tone", FS, 1.0, CENTER_HZ, None);
    for on in [false, true] {
        let data = dir.0.join(if on { "on" } else { "off" });
        let driver = MockSdrDriver::new(
            &meta,
            MockOptions {
                end: MockEnd::Loop,
                block_len: 65_536,
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
            &data,
            replay_plan(info.center_hz, info.sample_rate_hz, info.start_time),
        )
        .unwrap();
        cfg.source_class = window_class(CENTER_HZ, FS);
        cfg.live_window_class = true;
        cfg.lossless = source.pausable();
        assert!(!cfg.lossless);
        cfg.settings.chains = Some(Vec::new());
        cfg.iq_buffer = IqBufferConfig {
            enabled: Some(on),
            retention_s: 600.0,
            max_bytes: Some(1 << 30),
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
        let buffer = handle.iq_buffer();
        if on {
            assert!(buffer.wait_allocated(Duration::from_secs(60)));
            assert!(buffer.enabled(), "{:?}", buffer.status(None, None, 0));
        }
        let t = Instant::now();
        std::thread::sleep(Duration::from_secs_f64(secs));
        let s = buffer.status(None, None, 0);
        handle.stop();
        let (summary, fired) = wait_guarded(handle, Duration::from_secs(60));
        assert!(!fired);
        let elapsed = t.elapsed().as_secs_f64();
        let produced = summary.counter("/source/samples");
        let line = format!(
            "iq ring {}: {elapsed:.1} s, source {produced} samples ({:.2} Msps), always-on readers \
             lost {} (spectrum {}, detect {}, history {}), source ring errors {}; buffer: stored \
             {} samples (retained {}), dropped {} ({:.4} %), failed {}, write errors {}, sync \
             errors {}, poisoned {}, slots overwritten {}, wraps {}",
            if on { "ON " } else { "OFF" },
            produced as f64 / elapsed / 1e6,
            summary.always_on_lost_samples,
            summary.counter("/readers/spectrum/lost_samples"),
            summary.counter("/readers/detect/lost_samples"),
            summary.counter("/readers/history/lost_samples"),
            summary.counter("/source/ring_errors"),
            s.samples + s.evicted.samples,
            s.samples,
            s.dropped_samples,
            100.0 * s.dropped_samples as f64 / produced.max(1) as f64,
            s.failed_samples,
            s.write_errors,
            s.sync_errors,
            s.poisoned_samples,
            s.evicted.chunks,
            s.wrap_count,
        );
        eprintln!("{line}");
        if let Ok(out) = std::env::var("HK_MEASURE_OUT") {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(out)
                .unwrap();
            writeln!(f, "{line}").unwrap();
        }
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    }
}
