//! **T-939 — what the detection reader costs per sample, and whether 20 Msps fits.** `timing` tier.
//!
//! Found on live hardware: at 20 Msps the detection reader read about a third of the stream
//! (`readers.detect.lost_samples` 3.1 G, 39 overruns in ~4 minutes), the history reader lost
//! nothing, and the waterfall went mostly empty. Two questions follow, and neither can be answered
//! by reading the code: **where does the per-sample time go**, and **does a reader that cannot keep
//! up cost the capture thread anything** (it must not — the flow gate is off for a live source and
//! the ring never waits for a reader, so any coupling is CPU contention, not backpressure).
//!
//! So this drives the whole pipeline through the device interface with a **paced, dropping** front
//! end ([`paced::Paced`]) at 20 Msps and reports, per stage, the ns per sample each one costs. The
//! assertions are throughput bounds, which is why this is `timing` and not a gate test (docs/10
//! §3.6): they measure the box as much as the code. The per-stage numbers come from the counters
//! the reader keeps in production (`readers.detect.{clip,burst,stft,frame,wait}_ns`), so the same
//! profile is readable from `/api/status` on the Jetson or the Mac — this test is one way to read
//! them, not the only one.
//!
//! What it holds the code to:
//!
//! 1. **The reader is never lapped.** `lost_samples` stays 0. Loss it cannot avoid is *shed*: a
//!    deliberate skip to the live edge inside [the lag budget], counted as `shed_samples`. The
//!    difference is the whole point — an overrun is a ring-sized hole at an unpredictable moment,
//!    a shed is a small one at a bounded distance from the live edge, and only one of them can be
//!    reported honestly.
//! 2. **The reader stays near the live edge**, so a detection is at most a fraction of a second
//!    old whatever the machine can do.
//! 3. **A reader that sheds does not starve the others.** The history reader loses nothing and
//!    folds rows throughout — the waterfall is never gated on detection.

mod common;
#[path = "support/paced.rs"]
mod paced;
#[path = "support/radio.rs"]
mod radio;

use std::f64::consts::TAU;
use std::time::Duration;

use common::*;
use hk_model::Timestamp;
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use num_complex::Complex;
use serde_json::{Value, json};

/// 20 Msps — the HackRF's top rate and the span the report was filed against.
const FS: f64 = 20e6;
/// Away from the 902–928 MHz short-burst band, so this measures the ordinary profile.
const CENTER: f64 = 750e6;
/// Samples per transfer (a HackRF `hackrf_transfer` buffer is 262 144 bytes = 131 072 ci8 samples;
/// half of one keeps the pacing granularity at 3.3 ms).
const BLOCK: usize = 65_536;
/// Seconds of stream to measure over, after the warm-up.
const MEASURE_S: f64 = 8.0;
/// Seconds of stream discarded first (thread start-up, the first database writes, the namer).
const WARMUP_S: f64 = 2.0;

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

/// A scene with something to detect across the whole 20 MHz: 24 tones on a 700 kHz raster plus
/// broadband noise, 2^21 samples (105 ms) of it, phase-continuous across the wrap so the loop
/// itself is not a detection.
fn scene() -> Vec<Complex<i8>> {
    const LEN: usize = 1 << 21;
    let mut out = vec![Complex::new(0i8, 0i8); LEN];
    let mut re = vec![0f32; LEN];
    let mut im = vec![0f32; LEN];
    for k in 0..24u32 {
        // An integer number of cycles per buffer: continuous across the wrap.
        let cycles = (k as f64 * 700e3 / FS * LEN as f64).round() as u64 + 3;
        let amp = 8.0 + (k % 5) as f32 * 4.0;
        for (i, (r, m)) in re.iter_mut().zip(im.iter_mut()).enumerate() {
            let ph = TAU * (cycles * i as u64 % LEN as u64) as f64 / LEN as f64;
            *r += amp * ph.cos() as f32;
            *m += amp * ph.sin() as f32;
        }
    }
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for (o, (r, m)) in out.iter_mut().zip(re.iter().zip(im.iter())) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let noise = ((state >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 10.0;
        *o = Complex::new(
            (r + noise).round().clamp(-128.0, 127.0) as i8,
            (m - noise).round().clamp(-128.0, 127.0) as i8,
        );
    }
    out
}

/// Plays `iq` in a loop by contiguous copies (the per-sample modulo of `radio::looped` would be a
/// measurable share of 20 Msps on the source thread itself).
fn looped_fast(iq: Vec<Complex<i8>>) -> radio::Generator {
    Box::new(move |_, _, index, n, out| {
        let len = iq.len();
        let mut at = (index % len as u64) as usize;
        let mut left = n;
        while left > 0 {
            let take = left.min(len - at);
            out.extend_from_slice(&iq[at..at + take]);
            at = (at + take) % len;
            left -= take;
        }
    })
}

#[test]
fn the_detection_reader_keeps_up_with_a_20_msps_front_end() {
    let dir = TempDir::new("detect-throughput");
    let (rx, ctl, front) = paced::Paced::new(CENTER, FS, BLOCK, looped_fast(scene()));
    ctl.run_free();
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut plan = replay_plan(CENTER, FS, t0);
    plan.extra = json!({ "pipeline": { "ring_s": 4.0 } });
    let mut cfg = PipelineConfig::new(&dir.0, plan).unwrap();
    cfg.source_class = window_class(CENTER, FS);
    cfg.live_window_class = true;
    // A live front end cannot be paused, so the run takes the live path whatever this says; it is
    // set explicitly because that is the case under test.
    cfg.lossless = false;
    // No chains: this measures the always-on readers, which are what must fit before anything is
    // attached on top of them.
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
    let counters = handle.counters();

    std::thread::sleep(Duration::from_secs_f64(WARMUP_S));
    let warm = counters.to_json();
    std::thread::sleep(Duration::from_secs_f64(MEASURE_S));
    let hot = counters.to_json();
    handle.stop();
    let (summary, watchdog) = wait_guarded(handle, Duration::from_secs(120));
    assert!(!watchdog, "the run had to be stopped by the watchdog");

    let at = |v: &Value, path: &[&str]| -> u64 {
        let mut cur = v;
        for k in path {
            cur = &cur[*k];
        }
        n(cur)
    };
    let d = |path: &[&str]| at(&hot, path) - at(&warm, path);
    let det_samples = d(&["readers", "detect", "samples"]);
    let det_shed = d(&["readers", "detect", "shed_samples"]);
    let det_lost = d(&["readers", "detect", "lost_samples"]);
    let src_samples = d(&["source", "samples"]);
    let src_dropped = d(&["source", "source_dropped"]);
    let hist_samples = d(&["readers", "history", "samples"]);
    let hist_lost = d(&["readers", "history", "lost_samples"]);
    let hist_frames = d(&["readers", "history", "frames"]);

    // ---- the profile: ns per sample the reader actually read, per stage ----
    let per = |ns: u64| ns as f64 / det_samples.max(1) as f64;
    let stage = |k: &str| per(d(&["readers", "detect", k]));
    let stft_only =
        per(d(&["readers", "detect", "stft_ns"]) - d(&["readers", "detect", "frame_ns"]));
    println!(
        "\nT-939 detection reader at {:.0} Msps, {:.0} s of stream\n\
         \x20 source      {:>10} samples, {} dropped by the front end\n\
         \x20 detect      {:>10} samples read, {} shed, {} lost to overruns\n\
         \x20 history     {:>10} samples read, {} lost, {} frames\n\
         \x20 per sample the reader read:\n\
         \x20   clip scan   {:6.2} ns\n\
         \x20   burst       {:6.2} ns\n\
         \x20   stft        {:6.2} ns  (transform only)\n\
         \x20   frame       {:6.2} ns  (floor, detector, tracker, batching)\n\
         \x20   ---- total  {:6.2} ns   -> {:.1} Msps a core\n\
         \x20   waiting     {:6.2} ns  (ahead of the ring, plus the copy out of it)\n\
         \x20 capture thread {:6.2} ns/sample, detect thread cpu {:.2} cores\n",
        FS / 1e6,
        MEASURE_S,
        src_samples,
        src_dropped,
        det_samples,
        det_shed,
        det_lost,
        hist_samples,
        hist_lost,
        hist_frames,
        stage("clip_ns"),
        stage("burst_ns"),
        stft_only,
        stage("frame_ns"),
        stage("clip_ns") + stage("burst_ns") + stft_only + stage("frame_ns"),
        1e3 / (stage("clip_ns") + stage("burst_ns") + stft_only + stage("frame_ns")).max(1e-9),
        stage("wait_ns"),
        d(&["source", "cpu_ns"]) as f64 / src_samples.max(1) as f64,
        d(&["readers", "detect", "cpu_ns"]) as f64 / (MEASURE_S * 1e9),
    );

    println!(
        "    frame split: floor {:6.2} ns, detector {:6.2} ns, tracker+batch {:6.2} ns  \
         ({:.0} us/frame over {} frames)",
        stage("floor_ns"),
        stage("detector_ns"),
        stage("track_ns"),
        d(&["readers", "detect", "frame_ns"]) as f64
            / 1e3
            / d(&["readers", "detect", "frames"]).max(1) as f64,
        d(&["readers", "detect", "frames"]),
    );
    println!(
        "  shed events {}, writer_blocked {}, writer_queue_full {}, db_batches {}, detections {}, \
         dense {}, frames {}, stft_resets {}\n",
        d(&["readers", "detect", "shed_events"]),
        d(&["detect", "writer_blocked"]),
        d(&["detect", "writer_queue_full"]),
        d(&["detect", "db_batches"]),
        d(&["detect", "detections"]),
        d(&["detect", "dense_frames"]),
        d(&["readers", "detect", "frames"]),
        d(&["readers", "detect", "stft_resets"]),
    );
    assert!(
        src_samples > (0.8 * MEASURE_S * FS) as u64,
        "the front end did not stream at {FS} Hz: {src_samples} samples in {MEASURE_S} s"
    );
    // 1. Never lapped. Whatever the box can do, the reader gives ground on its own terms.
    assert_eq!(
        det_lost, 0,
        "the detection reader was lapped by the ring: {det_lost} samples lost in {MEASURE_S} s. \
         A reader that cannot keep up must SHED (bounded, counted, near the live edge), not be \
         overrun (a ring-sized hole at an unpredictable moment)"
    );
    // 2. It keeps up: everything the front end delivered was either analysed or shed, and
    //    essentially all of it analysed.
    let covered = det_samples + det_shed;
    assert!(
        covered as f64 >= 0.99 * src_samples as f64,
        "detection accounted for {covered} of {src_samples} samples"
    );
    // 90 %, measured. Three runs on the 24-core Linux dev box at load average 15–22 (release,
    // the box far from quiet) read 100 %, 95.4 % and 94.0 % analysed, at 29.6–42.7 ns per sample —
    // against the ~35 % the live 20 Msps HackRF run was getting. The bound is below the worst of
    // those because the shed is what a busy box is *supposed* to do; the regression this guards is
    // the reader falling back to a fraction of the stream, and `lost_samples == 0` above is the
    // sharp assertion.
    assert!(
        det_samples as f64 >= 0.90 * src_samples as f64,
        "the detection reader analysed only {:.1} % of a {:.0} Msps stream ({det_shed} shed). \
         The per-stage profile above says which stage to fix.",
        100.0 * det_samples as f64 / src_samples.max(1) as f64,
        FS / 1e6
    );
    // 3. It never costs the other readers anything: the waterfall is not gated on detection.
    assert_eq!(hist_lost, 0, "the history reader lost samples");
    assert!(
        hist_frames >= (0.5 * MEASURE_S * 10.0) as u64,
        "the history reader folded {hist_frames} frames in {MEASURE_S} s at 10 rows/s"
    );
    assert!(
        front.dropped() < (0.02 * MEASURE_S * FS) as u64,
        "the front end dropped {} samples: the capture thread could not collect transfers in time",
        front.dropped()
    );
    assert!(
        summary.counters["readers"]["detect"]["frames"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
}
