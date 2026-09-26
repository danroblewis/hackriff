//! T-1048 (LSR-7): the spectrum reader's row fold — dB conversion plus wire serialize
//! (`spectrum::Output::row`) — is measured, never assumed (T-453). Two tests, per docs/10 §3.6's
//! kind-2 split:
//!
//!  1. **Deterministic, gates normally**: after real rows have published, `/api/status`'s
//!     `spectrum` counters (`fold_ns_last`, `fold_ns_max`, `fold_ns_total`) are non-zero and
//!     mutually consistent. This is the "measured, never assumed" claim — it does not depend on
//!     the numbers being small, fast or any particular size, only on them existing.
//!  2. **Timing tier** (`.config/nextest.toml`'s `default-filter`; run by `just timing`): collects
//!     one real per-row sample for many rows — never a synthetic loop — and prints p50/p95/max.
//!     **No bound is asserted on the wall-clock numbers themselves**: how fast this fold runs is a
//!     property of the machine, exactly the reason the tier exists.
//!
//! Both drive a real `Pipeline` with the scripted radio (`support/radio.rs`, T-050/T-057's own
//! harness) rather than calling `spectrum::Output` directly, which is private to the crate: the
//! fold this measures is the one the wire actually pays for.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use hk_model::{ContentClass, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};

const FS: f64 = 250e3;
const CENTER: f64 = 100.5e6;

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    while !f() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A steady, un-retuned tone — this file is about the fold's OWN cost, not about a tune path
/// (`spectrum_header_follows_tune.rs` already covers that).
fn start(
    dir: &TempDir,
) -> (
    hk_pipeline::PipelineHandle,
    std::sync::Arc<radio::RadioControl>,
) {
    assert_eq!(window_class(CENTER, FS), ContentClass::Unrestricted);
    let (radio, ctl) = radio::Radio::new(CENTER, FS, 256, radio::tone(|_| 50e3));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(CENTER, FS, t0)).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    cfg.settings.spectrum_fft_len = 256;
    // A real subscriber, discarded: `spectrum::Output::watched()` (T-489) skips the STFT entirely
    // while nothing is attached, and this file is about the fold's cost, not about proving T-489's
    // skip. `spectrum_header_follows_tune.rs` attaches one for the same reason.
    let id = cfg.spectrum_stream_id.clone();
    cfg.stream_sink = Some(std::sync::Arc::new(
        move |h: &hk_stream::StreamHeader, handle: hk_stream::PublisherHandle| {
            if h.stream_id != id {
                return;
            }
            let _ = handle.subscribe(
                "t1048-fold",
                hk_stream::Declared::local(std::io::sink()),
                Box::new(|_| {}),
            );
        },
    ));
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
    (handle, ctl)
}

#[test]
fn spectrum_fold_cost_is_measured_and_reported() {
    let dir = TempDir::new("t1048-fold-det");
    let (handle, ctl) = start(&dir);
    let counters = handle.counters();

    // 2 s of stream is comfortably more than one row at any plausible row rate.
    let n = FS as u64 * 2;
    ctl.hold_at(n);
    assert!(
        ctl.wait_emitted(n, Duration::from_secs(60)),
        "the radio never emitted {n} samples"
    );
    wait(
        "at least a few spectrum rows to publish",
        Duration::from_secs(60),
        || counters.spectrum.rows.load(Ordering::Relaxed) > 3,
    );
    ctl.finish();
    let (summary, fired) = wait_guarded(handle, Duration::from_secs(60));
    assert!(!fired, "the run did not finish on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let rows = counters.spectrum.rows.load(Ordering::Relaxed);
    let last = counters.spectrum.fold_ns_last.load(Ordering::Relaxed);
    let max = counters.spectrum.fold_ns_max.load(Ordering::Relaxed);
    let total = counters.spectrum.fold_ns_total.load(Ordering::Relaxed);
    assert!(rows > 3, "no spectrum row was published: {rows}");
    assert!(
        last > 0,
        "the fold's own cost was never timed (fold_ns_last == 0), so it is assumed, not measured"
    );
    assert!(
        max >= last,
        "fold_ns_max ({max}) must be at least the newest sample ({last})"
    );
    assert!(
        total >= max,
        "fold_ns_total ({total}) must be at least its own max ({max})"
    );
    eprintln!(
        "spectrum fold cost over {rows} rows: last {last} ns, max {max} ns, mean {} ns",
        total / rows.max(1)
    );
}

/// **Timing tier** — `just timing` only (`.config/nextest.toml`). Collects one real fold-cost
/// sample per row by releasing the radio progressively more samples until a new row lands, then
/// reading `fold_ns_last` (never a blind poll racing a live run, and never a synthetic loop calling
/// the fold in isolation): the value read is the newest row's own, deterministically, because no
/// further sample is released until this sample is taken.
#[test]
fn the_spectrum_fold_cost_per_row_p50_and_p95_are_printed() {
    let dir = TempDir::new("t1048-fold-timing");
    let (handle, ctl) = start(&dir);
    let counters = handle.counters();

    const TARGET_SAMPLES: usize = 150;
    let chunk = (FS / 50.0) as u64; // ~20 ms of stream per release
    let mut samples = Vec::with_capacity(TARGET_SAMPLES);
    let mut emitted = 0u64;
    for _ in 0..TARGET_SAMPLES {
        let start_rows = counters.spectrum.rows.load(Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            emitted += chunk;
            ctl.hold_at(emitted);
            assert!(
                ctl.wait_emitted(emitted, Duration::from_secs(30)),
                "the radio never emitted {emitted} samples"
            );
            if counters.spectrum.rows.load(Ordering::Relaxed) > start_rows {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out releasing samples toward a new spectrum row (at {emitted}, rows still {start_rows})"
            );
        }
        samples.push(counters.spectrum.fold_ns_last.load(Ordering::Relaxed));
    }
    ctl.finish();
    let (summary, fired) = wait_guarded(handle, Duration::from_secs(60));
    assert!(!fired, "the run did not finish on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    assert_eq!(samples.len(), TARGET_SAMPLES);
    assert!(
        samples.iter().all(|&s| s > 0),
        "a sampled fold cost was zero, which is not a real timing: {samples:?}"
    );
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    let max = *samples.last().unwrap();
    let mean = samples.iter().sum::<u64>() / samples.len() as u64;
    eprintln!(
        "spectrum fold cost per row, n={}: mean {mean} ns, p50 {p50} ns, p95 {p95} ns, max {max} ns",
        samples.len()
    );
    // Deliberately no bound: this tier measures the machine's headroom, docs/10 §3.6 kind 2 — a
    // gate assertion on this number is exactly what the tier exists to keep out of the gate.
}
