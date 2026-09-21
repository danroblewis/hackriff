//! T-056: compute providers through the composed pipeline (ADR-0007).
//!
//! - **Parity (SPACE-050 / AWARE-042 readers).** The same recording runs once with
//!   `compute.provider = "cpu"` and once with the default (`auto`).
//!   - **Default build:** `auto` resolves to the CPU reference, and frames, history ingest, spectrum
//!     rows and stored detections are bit-identical.
//!   - **Build with the Mac providers** (`--features gpu-wgpu,accelerate`): `auto` runs the STFT
//!     rows on the GPU or Accelerate. Frame counts stay exact, because framing does not depend on
//!     the provider and the readers flush at stream end. Detections match within the conformance
//!     tolerance.
//! - **Live-safe.** A retune that re-plumbs the run (another rate) keeps every reader on the
//!   provider it started with: one `Compute` per run, and `provider_changes` stays 0.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_dsp::compute::ProviderKind;
use hk_model::{Detection, FreqRange, Region, TimeRange, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, RunSummary, SourceInfo, TrackInventory, replay_plan};
use hk_stream::{Declared, StreamKind};
use serde_json::{Value, json};

const READERS: [&str; 3] = ["detect", "history", "spectrum"];

/// An operator's `HK_COMPUTE*` would override the plan's provider and void the comparison.
fn env_overrides() -> bool {
    [
        "HK_COMPUTE",
        "HK_COMPUTE_STFT",
        "HK_COMPUTE_PFB",
        "HK_GPU_IN_FLIGHT",
    ]
    .iter()
    .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

/// [`common::run`] with a consumer attached to the spectrum stream.
///
/// **T-489:** the spectrum reader skips its STFT while nothing is subscribed, so an unwatched run
/// produces no spectrum frames at all — and this test is about the three readers' STFTs agreeing
/// across compute providers, which needs all three to run. The other two readers are always-on
/// regardless.
fn run_watched(dir: &Path, meta: &Path, extra: Value) -> RunSummary {
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let (mut cfg, replay) = replay_config(dir, meta, extra, hk_core::Pacing::Unpaced);
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.kind == StreamKind::Spectrum {
            handle
                .subscribe(
                    "t056-watcher",
                    Declared::local(Sink::default()),
                    Box::new(|_| {}),
                )
                .unwrap();
        }
    }));
    let summary = start(cfg, replay).wait().unwrap();
    eprintln!("{}", summary.to_text());
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    summary
}

fn provider(s: &RunSummary, reader: &str) -> String {
    s.counters
        .pointer(&format!("/compute/stft/{reader}/provider"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no {reader} selection in {}", s.counters["compute"]))
        .to_owned()
}

fn detections(dir: &Path) -> Vec<Detection> {
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    let mut d = repo(dir)
        .detections_in_region(&Region::new(FreqRange::new(0.0, 7e9), ever))
        .unwrap();
    d.sort_by_key(|x| (x.time.start.as_unix_nanos(), x.f_center_hz.to_bits()));
    d
}

/// A detection without its per-run identities.
fn measured(d: &Detection) -> Value {
    let mut v = serde_json::to_value(d).unwrap();
    let o = v.as_object_mut().unwrap();
    for k in ["id", "survey_id", "provenance_ref"] {
        o.remove(k);
    }
    v
}

#[test]
fn the_default_provider_matches_the_cpu_reference() {
    if env_overrides() {
        eprintln!("SKIP: HK_COMPUTE* is set");
        return;
    }
    let dir = TempDir::new("t056-parity");
    let src = tone_recording(&dir.0.join("src"), "tone", 2.4e6, 1.0, 433.5e6, None);
    let pipeline = |compute: Value| json!({ "pipeline": { "chains": [], "compute": compute } });
    let cpu_dir = dir.0.join("cpu");
    let cpu = run_watched(&cpu_dir, &src, pipeline(json!({ "provider": "cpu" })));
    let auto_dir = dir.0.join("auto");
    let auto = run_watched(&auto_dir, &src, pipeline(json!({})));

    for s in [&cpu, &auto] {
        assert_eq!(s.always_on_lost_samples, 0);
        assert_eq!(s.counter("/compute/provider_changes"), 0);
        assert_eq!(s.counter("/compute/stft_builds"), 3, "one STFT per reader");
        assert_eq!(
            s.counters["compute"]["providers"].as_array().map(Vec::len),
            Some(ProviderKind::ALL.len()),
            "every provider's availability is reported"
        );
    }
    assert_eq!(cpu.counters["compute"]["options"]["provider"], "cpu");
    assert_eq!(auto.counters["compute"]["options"]["provider"], "auto");
    let auto_provider = provider(&auto, "detect");
    for r in READERS {
        assert_eq!(provider(&cpu, r), "cpu", "{r}");
        assert_eq!(
            provider(&auto, r),
            auto_provider,
            "{r}: one provider per workload"
        );
        let frames = format!("/readers/{r}/frames");
        assert!(cpu.counter(&frames) > 0, "{r}");
        assert_eq!(cpu.counter(&frames), auto.counter(&frames), "{r} frames");
    }
    if !ProviderKind::GpuWgpu.compiled() && !ProviderKind::Accelerate.compiled() {
        assert_eq!(
            auto_provider, "cpu",
            "a default build resolves auto to the CPU"
        );
    }

    let (want, got) = (detections(&cpu_dir), detections(&auto_dir));
    assert!(!want.is_empty(), "the tone is detected");
    eprintln!(
        "T-056 parity: auto -> {auto_provider}: {} detections vs {} on the CPU reference",
        got.len(),
        want.len()
    );
    if auto_provider == "cpu" {
        let (w, g): (Vec<_>, Vec<_>) = (
            want.iter().map(measured).collect(),
            got.iter().map(measured).collect(),
        );
        assert_eq!(w, g, "bit-identical detections");
        for p in [
            "/history/frames_ingested",
            "/spectrum/rows",
            "/detect/tracks_opened",
        ] {
            assert_eq!(cpu.counter(p), auto.counter(p), "{p}");
        }
    } else {
        // Conformant providers agree with the reference within 0.01 dB per PSD bin: the same
        // emissions, in the same place; a threshold-edge detection may differ.
        let slack = 1.max(want.len() / 20);
        assert!(
            got.len().abs_diff(want.len()) <= slack,
            "{} vs {} detections",
            got.len(),
            want.len()
        );
        let bin = 2.4e6 / auto.resolution.fft_len as f64;
        let strongest = |d: &[Detection]| {
            d.iter()
                .max_by(|a, b| a.snr_peak_db.total_cmp(&b.snr_peak_db))
                .map(|x| x.f_center_hz)
                .unwrap()
        };
        assert!((strongest(&want) - strongest(&got)).abs() <= bin);
        assert_eq!(
            cpu.counter("/history/frames_ingested"),
            auto.counter("/history/frames_ingested")
        );
    }
}

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_re_plumb_keeps_every_reader_on_its_provider() {
    const CENTER: f64 = 433.5e6;
    const FS: f64 = 1e6;
    let dir = TempDir::new("t056-replumb");
    let (radio, ctl) = radio::Radio::new(CENTER, FS, 4096, radio::tone(|_| 50e3));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(CENTER, FS, t0)).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    let first = (FS / 2.0) as u64;
    ctl.hold_at(first);
    let handle = Pipeline::start(
        cfg,
        Box::new(radio),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: CENTER,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    assert!(ctl.wait_emitted(first, Duration::from_secs(60)));
    wait("the first segment's rows", Duration::from_secs(60), || {
        counters.spectrum_reader.samples.load(Ordering::Relaxed) >= first
    });
    let before: Vec<Option<String>> = READERS
        .iter()
        .map(|r| counters.compute.stft_provider(r))
        .collect();
    assert!(before.iter().all(Option::is_some), "{before:?}");
    let out = handle
        .controller()
        .retune(CENTER, 2.0 * FS)
        .expect("retune");
    assert!(out.replumbed, "another rate: re-plumbed");
    let second = first + (2.0 * FS / 2.0) as u64;
    ctl.hold_at(second);
    assert!(ctl.wait_emitted(second, Duration::from_secs(60)));
    ctl.finish();
    let (summary, fired) = wait_guarded(handle, Duration::from_secs(120));
    eprintln!("{}", summary.to_text());
    assert!(!fired, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);
    assert!(
        summary.counter("/compute/stft_builds") >= 6,
        "two segments x three readers: {}",
        summary.counters["compute"]
    );
    assert_eq!(summary.counter("/compute/provider_changes"), 0);
    let after: Vec<Option<String>> = READERS
        .iter()
        .map(|r| counters.compute.stft_provider(r))
        .collect();
    assert_eq!(before, after, "no provider change mid-run");
    assert!(summary.to_text().contains("compute:"));
}
