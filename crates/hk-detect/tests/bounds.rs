//! Worst-case frames stay bounded in memory and time (T-006 review): a time–frequency
//! checkerboard, 2048 one-bin lines (dense), 1024 lines with a 2-frame gap (the gap-merge worst
//! case under the run cap), and 1024 alternating one-frame runs. Exact profiles (no noise) at
//! 4096 bins with a known floor; cells at +10 dB are floor-branch seeds.

mod common;

use std::time::Instant;

use common::*;
use hk_core::Discontinuity;
use hk_detect::DetectorConfig;
use hk_model::SurveyId;

/// Per-frame cost bound at 4096 bins (a frame is 2.048 ms at 20 Msps).
const MAX_MEAN_MS: f64 = 5.0;
/// Labeller memory bound.
const MAX_MEMORY_BYTES: usize = 16 << 20;

struct Result {
    mean_ms: f64,
    worst_ms: f64,
    memory_mid: usize,
    memory_end: usize,
    dense: u64,
    dropped: u64,
    live_max: usize,
}

fn run(name: &str, frames: u64, cell: impl Fn(u64, usize) -> bool) -> Result {
    let mut s = Scene::new(
        DetectorConfig::new(SurveyId::new()),
        GammaFrames::new(BINS, N_AVG, provenance(98e6, FS, 24.0), 1),
    );
    let mut profile = flat(BINS);
    let (mut total, mut worst) = (0.0f64, 0.0f64);
    let (mut memory_mid, mut live_max) = (0usize, 0usize);
    let warm = frames / 5;
    for t in 0..frames {
        for (b, p) in profile.iter_mut().enumerate() {
            *p = if cell(t, b) { 10.0 } else { 1.0 };
        }
        let start = Instant::now();
        s.step_exact(&profile, Discontinuity::NONE);
        let dt = start.elapsed().as_secs_f64() * 1e3;
        s.out.detections.clear();
        if t >= warm {
            total += dt;
            worst = worst.max(dt);
        }
        live_max = live_max.max(s.det.live_components());
        if t == frames / 2 {
            memory_mid = s.det.labeler_memory_bytes();
        }
    }
    let r = Result {
        mean_ms: total / (frames - warm) as f64,
        worst_ms: worst,
        memory_mid,
        memory_end: s.det.labeler_memory_bytes(),
        dense: s.det.stats().dense_frames,
        dropped: s.det.stats().dropped_runs,
        live_max,
    };
    eprintln!(
        "{name}: mean {:.3} ms/frame (worst {:.2}), labeller memory {:.2} MB mid / {:.2} MB end, dense frames {}, dropped runs {}, live max {}",
        r.mean_ms,
        r.worst_ms,
        r.memory_mid as f64 / 1e6,
        r.memory_end as f64 / 1e6,
        r.dense,
        r.dropped,
        r.live_max
    );
    r
}

fn check(r: &Result) {
    assert!(r.mean_ms < MAX_MEAN_MS, "mean {} ms/frame", r.mean_ms);
    assert!(
        r.memory_end < MAX_MEMORY_BYTES,
        "memory {} bytes",
        r.memory_end
    );
    assert!(
        r.memory_end <= r.memory_mid + r.memory_mid / 10 + 4096,
        "memory still growing: {} → {}",
        r.memory_mid,
        r.memory_end
    );
}

#[test]
fn checkerboard_frames_are_dense_and_bounded() {
    let r = run("checkerboard", 600, |t, b| (b as u64 + t) % 2 == 0);
    check(&r);
    assert_eq!(r.dense, 600, "2048 runs per frame exceed the 1024-run cap");
    assert_eq!(r.live_max, 0);
}

#[test]
fn many_lines_with_a_two_frame_gap_are_bounded() {
    // 2048 lines (every other bin), 3 frames on / 2 off: dense on the on-frames.
    let r = run("2048 lines, gap 2", 600, |t, b| b % 2 == 0 && t % 5 < 3);
    check(&r);
    assert_eq!(r.dense, 360);
    // 1024 lines (every 4th bin): under the cap, every line is one component merged across its
    // 2-frame gaps, split each second.
    let r = run("1024 lines, gap 2", 600, |t, b| b % 4 == 0 && t % 5 < 3);
    check(&r);
    assert_eq!(r.dense, 0);
    assert!(r.live_max <= 2 * 1024 + 8, "{}", r.live_max);
}

#[test]
fn alternating_one_frame_runs_are_bounded() {
    // 1024 one-bin, one-frame runs that shift by 2 bins each frame: nothing is ever kept, but
    // every run links to the run two frames back (the same bins).
    let r = run("1024 alternating runs", 600, |t, b| {
        (b as u64 + 2 * t) % 4 == 0
    });
    check(&r);
    assert_eq!(r.dense, 0);
    assert_eq!(r.dropped, 0);
}
