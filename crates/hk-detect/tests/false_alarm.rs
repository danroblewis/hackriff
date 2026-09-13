//! False-alarm bounds (S4 §3.3): the recommended chain on ideal `Gamma(10)` frames (≥ 1 MHz·h),
//! the regressions S4 found (fixed −3 dB hysteresis, closing before the duration test), the full
//! chain with the real floor tracker, and sloped floors (T-005 re-review) with the per-frame
//! reference.
//!
//! Exposure follows S4 `synth_chain_fa.py`: 4096 bins at 20 Msps (4.88 kHz), 10 averages
//! (2.048 ms frames), boxes within 64 bins of either band edge excluded; the 95 % upper bound on
//! the rate is `Gamma⁻¹(k + 1, 0.95)` events over the exposure.

mod common;

use common::*;
use hk_core::Discontinuity;
use hk_detect::{ClipCount, DetectionProfile, Detector, DetectorConfig, Hysteresis};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker, gamma};
use hk_model::SurveyId;

const EDGE_EXCLUDE: usize = 64;

fn exposure_mhz_h(bins: usize, fs: f64, frames: u64, frame_s: f64) -> f64 {
    let mhz = (bins - 2 * EDGE_EXCLUDE) as f64 * fs / bins as f64 / 1e6;
    mhz * frames as f64 * frame_s / 3600.0
}

fn upper95(k: u64) -> f64 {
    gamma::inverse_lower(k as f64 + 1.0, 0.95)
}

#[derive(Default, Clone, Copy)]
struct Noise {
    interior_boxes: u64,
    all_boxes: u64,
    region_cells: u64,
    cells: u64,
}

/// Known-floor Gamma(10) noise through the detector.
fn run_noise(config: DetectorConfig, frames: u64, seed: u64) -> Noise {
    let prov = provenance(98e6, FS, 24.0);
    let mut src = GammaFrames::new(BINS, N_AVG, prov, seed);
    let mut det = Detector::new(config).unwrap();
    let mut frame = src.empty_frame();
    let mut floor = floor_frame(&frame, &flat(BINS), 0);
    let profile = flat(BINS);
    let mut n = Noise::default();
    let count = |e: hk_detect::DetectorEvent<'_>, n: &mut Noise| {
        if let hk_detect::DetectorEvent::Detection(d) = e {
            n.all_boxes += 1;
            if d.bins.start >= EDGE_EXCLUDE && d.bins.end <= BINS - EDGE_EXCLUDE {
                n.interior_boxes += 1;
            }
        }
    };
    for i in 0..frames {
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        src.fill(&mut frame, &profile, flags);
        refresh_floor(&mut floor, &frame, 0, false);
        det.process(&frame, &floor, ClipCount::NONE, &mut |e| count(e, &mut n));
        n.region_cells += det.last_classify().region as u64;
        n.cells += BINS as u64;
    }
    det.finish(&mut |e| count(e, &mut n));
    n
}

fn run_parallel(config: &DetectorConfig, total_frames: u64, seed: u64) -> (Noise, u64) {
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(2, 8) as u64;
    let per = total_frames.div_ceil(threads);
    let mut sum = Noise::default();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let c = config.clone();
                scope.spawn(move || run_noise(c, per, seed + 7919 * t))
            })
            .collect();
        for h in handles {
            let n = h.join().unwrap();
            sum.interior_boxes += n.interior_boxes;
            sum.all_boxes += n.all_boxes;
            sum.region_cells += n.region_cells;
            sum.cells += n.cells;
        }
    });
    (sum, per * threads)
}

#[test]
fn recommended_chain_has_no_false_boxes_on_ideal_gamma_frames_over_1_mhz_hour() {
    let frame_s = (BINS * N_AVG as usize) as f64 / FS;
    let per_frame = exposure_mhz_h(BINS, FS, 1, frame_s);
    let target = 1.1;
    let frames = (target / per_frame).ceil() as u64;
    let config = DetectorConfig::new(SurveyId::new());
    let start = std::time::Instant::now();
    let (n, frames) = run_parallel(&config, frames, 0x5eed_0006);
    let exposure = exposure_mhz_h(BINS, FS, frames, frame_s);
    eprintln!(
        "ideal Gamma(10), OR on 1e-6 / off 1e-3 / min 3 / gap 2: {} interior false boxes ({} incl. edges) in {exposure:.3} MHz·h \
         ({frames} frames); rate {:.2} /MHz/h, 95 % upper bound {:.2} /MHz/h; region cells {:.2e} per cell; {:.1} s",
        n.interior_boxes,
        n.all_boxes,
        n.interior_boxes as f64 / exposure,
        upper95(n.interior_boxes) / exposure,
        n.region_cells as f64 / n.cells as f64,
        start.elapsed().as_secs_f64()
    );
    assert!(exposure >= 1.0);
    assert_eq!(n.interior_boxes, 0, "false boxes on ideal noise");
}

#[test]
fn fixed_minus_3_db_hysteresis_regression_percolates() {
    // S4: fixed −3 dB hysteresis (min 2) gave 35.5 false boxes/MHz/h; Pfa-derived off (min 2)
    // gave 0.76. The off level sits only ≈ 2 dB above mean noise.
    let frame_s = (BINS * N_AVG as usize) as f64 / FS;
    let frames = (0.3 / exposure_mhz_h(BINS, FS, 1, frame_s)).ceil() as u64;
    let mut fixed = DetectorConfig::new(SurveyId::new());
    fixed.profile = DetectionProfile {
        name: "fixed-3db-regression".into(),
        hysteresis: Hysteresis::FixedDb(3.0),
        min_frames: 2,
        ..DetectionProfile::standard()
    };
    let mut own = DetectorConfig::new(SurveyId::new());
    own.profile.min_frames = 2;
    let (nf, frames) = run_parallel(&fixed, frames, 0xf1_7ed);
    let (no, _) = run_parallel(&own, frames, 0xf1_7ed);
    let exposure = exposure_mhz_h(BINS, FS, frames, frame_s);
    eprintln!(
        "fixed −3 dB: {} false boxes ({:.1} /MHz/h), region fraction {:.2e}; own Pfa off: {} boxes ({:.1} /MHz/h), region fraction {:.2e}; {exposure:.3} MHz·h",
        nf.interior_boxes,
        nf.interior_boxes as f64 / exposure,
        nf.region_cells as f64 / nf.cells as f64,
        no.interior_boxes,
        no.interior_boxes as f64 / exposure,
        no.region_cells as f64 / no.cells as f64,
    );
    assert!(
        nf.interior_boxes >= 3,
        "the fixed-hysteresis bug did not reproduce"
    );
    assert!(nf.interior_boxes > 3 * no.interior_boxes.max(1));
    assert!(nf.region_cells > 5 * no.region_cells);
}

#[test]
fn min_duration_is_tested_before_gap_merge_regression() {
    // Single-frame +10 dB seeds every other frame at one bin: closing first would make a 9-frame
    // component; testing duration on raw components drops every one of them.
    let fc = 98e6;
    let run = |pattern: &dyn Fn(usize) -> bool, frames: usize| {
        let mut s = Scene::new(
            DetectorConfig::new(SurveyId::new()),
            GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), 1),
        );
        let quiet = flat(BINS);
        let mut hot = flat(BINS);
        hot[1000] = undb(10.0) as f32;
        for i in 0..frames {
            s.step_exact(if pattern(i) { &hot } else { &quiet }, Discontinuity::NONE);
        }
        s.finish();
        s.out.detections
    };
    assert!(run(&|i| i % 2 == 0 && i < 9, 14).is_empty());
    // Control: two 3-frame bursts 2 frames apart are kept and merged into one box.
    let d = run(&|i| (0..3).contains(&i) || (5..8).contains(&i), 14);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].merged_boxes, 2);
    assert_eq!(d[0].frames.end - d[0].frames.start, 8);
}

/// Result of [`tracked_noise_with`].
#[derive(Clone, Copy, Default)]
struct Tracked {
    /// Interior false boxes.
    boxes: u64,
    /// Floor-branch `T_off` exceedance ÷ design (1e-3) over the usable span, per-frame reference,
    /// counted only where the detector let the floor branch run (the step guard).
    ratio_frame: f64,
    /// The same for the wide reference, everywhere (informational).
    ratio_wide: f64,
    /// Fraction of usable-span cells where the floor branch was off.
    guarded: f64,
    frames: u64,
}

impl Tracked {
    fn add(&mut self, o: &Tracked) {
        let w = |a: f64, fa: u64, b: f64, fb: u64| {
            (a * fa as f64 + b * fb as f64) / (fa + fb).max(1) as f64
        };
        self.ratio_frame = w(self.ratio_frame, self.frames, o.ratio_frame, o.frames);
        self.ratio_wide = w(self.ratio_wide, self.frames, o.ratio_wide, o.frames);
        self.guarded = w(self.guarded, self.frames, o.guarded, o.frames);
        self.boxes += o.boxes;
        self.frames += o.frames;
    }

    fn exposure(&self) -> f64 {
        exposure_mhz_h(BINS, FS, self.frames, (BINS * N_AVG as usize) as f64 / FS)
    }
}

/// Gamma frames with per-bin mean `profile` through the real floor tracker and the detector.
fn tracked_noise_with(profile: &[f32], frames: u64, seed: u64, config: DetectorConfig) -> Tracked {
    let prov = provenance(98e6, FS, 24.0);
    let mut src = GammaFrames::new(BINS, N_AVG, prov, seed);
    let mut det = Detector::new(config).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut frame = src.empty_frame();
    let mut boxes = 0u64;
    let t_off = gamma::mean_threshold(10.0, 1e-3) as f32;
    let usable = (8e6 / (FS / BINS as f64)) as usize;
    let (lo, hi) = (BINS / 2 - usable, BINS / 2 + usable);
    let (mut exc_frame, mut exc_wide, mut cells, mut open, mut all) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let count = |e: hk_detect::DetectorEvent<'_>, boxes: &mut u64| {
        if let hk_detect::DetectorEvent::Detection(d) = e
            && d.bins.start >= EDGE_EXCLUDE
            && d.bins.end <= BINS - EDGE_EXCLUDE
        {
            *boxes += 1;
        }
    };
    for i in 0..frames {
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        src.fill(&mut frame, profile, flags);
        let f = tracker.update(&frame, |_| {});
        det.process(&frame, f, ClipCount::NONE, &mut |e| count(e, &mut boxes));
        if i >= 16 {
            let mask = det.floor_branch_mask();
            for b in lo..hi {
                let p = frame.spectrum.psd[b];
                exc_wide += u64::from(p > t_off * f.wide_floor[b]);
                if mask.is_none_or(|m| m[b]) {
                    exc_frame += u64::from(p > t_off * f.floor[b]);
                    open += 1;
                }
            }
            cells += (hi - lo) as u64;
            all += (hi - lo) as u64;
        }
    }
    det.finish(&mut |e| count(e, &mut boxes));
    Tracked {
        boxes,
        ratio_frame: exc_frame as f64 / open.max(1) as f64 / 1e-3,
        ratio_wide: exc_wide as f64 / cells as f64 / 1e-3,
        guarded: 1.0 - open as f64 / all.max(1) as f64,
        frames,
    }
}

#[test]
fn full_chain_with_the_floor_tracker_has_no_false_boxes_over_1_mhz_hour() {
    let frame_s = (BINS * N_AVG as usize) as f64 / FS;
    let target = (1.02 / exposure_mhz_h(BINS, FS, 1, frame_s)).ceil() as u64;
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(2, 8) as u64;
    let per = target.div_ceil(threads);
    let start = std::time::Instant::now();
    let mut sum = Tracked::default();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                scope.spawn(move || {
                    tracked_noise_with(
                        &flat(BINS),
                        per,
                        0xc4a1 + 7919 * t,
                        DetectorConfig::new(SurveyId::new()),
                    )
                })
            })
            .collect();
        for h in handles {
            sum.add(&h.join().unwrap());
        }
    });
    let exposure = sum.exposure();
    eprintln!(
        "tracker + detector, flat Gamma(10): {} false boxes in {exposure:.3} MHz·h ({} frames, {threads} threads, {:.1} s); \
         95 % bound {:.2} /MHz/h; floor-branch T_off exceedance ÷ design: per-frame {:.2}×, wide {:.2}×; guarded {:.2} %",
        sum.boxes,
        sum.frames,
        start.elapsed().as_secs_f64(),
        upper95(sum.boxes) / exposure,
        sum.ratio_frame,
        sum.ratio_wide,
        sum.guarded * 100.0
    );
    assert!(exposure >= 1.0);
    assert_eq!(sum.boxes, 0);
    assert!(sum.ratio_frame < 2.0, "{}", sum.ratio_frame);
}

fn tilt(db_span: f64) -> Vec<f32> {
    (0..BINS)
        .map(|b| undb(db_span * (b as f64 / (BINS - 1) as f64 - 0.5)) as f32)
        .collect()
}

/// Flat to ±6.5 MHz, then a raised-cosine drop of 20 dB by ±10 MHz (HackRF-like baseband roll-off).
fn rolloff() -> Vec<f32> {
    let df = FS / BINS as f64;
    (0..BINS)
        .map(|b| {
            let f = ((b as f64 - (BINS / 2) as f64) * df).abs();
            let x = ((f - 6.5e6) / 3.5e6).clamp(0.0, 1.0);
            let drop_db = 20.0 * 0.5 * (1.0 - (std::f64::consts::PI * x).cos());
            undb(-drop_db) as f32
        })
        .collect()
}

/// A notch `width` bins wide and `depth_db` deep at +3 MHz.
fn notch(width: usize, depth_db: f64) -> Vec<f32> {
    let mut p = flat(BINS);
    let c = BINS / 2 + (3e6 / (FS / BINS as f64)) as usize;
    for v in &mut p[c - width / 2..c + width / 2] {
        *v = undb(depth_db) as f32;
    }
    p
}

fn step_cases() -> Vec<(String, Vec<f32>)> {
    vec![
        ("-20 dB notch 16 bins".into(), notch(16, -20.0)),
        ("-20 dB notch 64 bins".into(), notch(64, -20.0)),
        ("-10 dB notch 64 bins".into(), notch(64, -10.0)),
        ("-20 dB notch 1024 bins".into(), notch(1024, -20.0)),
    ]
}

#[test]
fn sloped_floors_keep_the_false_alarm_bound_with_the_per_frame_reference() {
    let cases: Vec<(String, Vec<f32>)> = vec![
        ("6 dB tilt".into(), tilt(6.0)),
        ("12 dB tilt".into(), tilt(12.0)),
        ("baseband roll-off".into(), rolloff()),
    ];
    check_floor_cases(&cases, DetectorConfig::new(SurveyId::new()), true);
}

#[test]
fn floor_steps_keep_the_false_alarm_bound_with_the_per_frame_reference() {
    // The floor-step guard switches the floor branch off around the notch edges (block FCME is
    // biased there); where the floor branch still runs, exceedance is at design.
    check_floor_cases(&step_cases(), DetectorConfig::new(SurveyId::new()), true);
}

#[test]
fn floor_steps_without_the_step_guard_break_the_bound() {
    // Regression: without the guard the per-frame floor branch runs far above design at a notch.
    let mut config = DetectorConfig::new(SurveyId::new());
    config.step_guard = None;
    let t = tracked_noise_with(&notch(64, -20.0), 400, 0x51_0e, config);
    eprintln!(
        "-20 dB notch 64 bins, no step guard: {} false boxes, per-frame exceedance {:.1}×",
        t.boxes, t.ratio_frame
    );
    assert!(t.ratio_frame > 10.0, "{}", t.ratio_frame);
}

#[test]
fn floor_steps_with_the_os_only_branch() {
    // The OS branch adapts within its 32 reference cells, so a band with known floor steps (a
    // notch filter, a path switch) can also select `Branches::OsOnly` per profile.
    let mut config = DetectorConfig::new(SurveyId::new());
    config.profile.branches = hk_detect::Branches::OsOnly;
    check_floor_cases(&step_cases(), config, false);
}

/// Asserts 0 false boxes, and (when the floor branch is in use) a floor-branch `T_off`
/// exceedance within 3× design where the floor branch runs over the usable span.
fn check_floor_cases(cases: &[(String, Vec<f32>)], config: DetectorConfig, floor_branch: bool) {
    let mut failures = Vec::new();
    for (i, (name, profile)) in cases.iter().enumerate() {
        let t = tracked_noise_with(profile, 1500, 0x51_0e + i as u64, config.clone());
        eprintln!(
            "{name}: {} false boxes in {:.3} MHz·h; floor-branch T_off exceedance ÷ design where it runs: \
             per-frame {:.2}× (floor branch off on {:.1} % of the usable span); wide reference {:.1}×",
            t.boxes,
            t.exposure(),
            t.ratio_frame,
            t.guarded * 100.0,
            t.ratio_wide
        );
        if t.boxes > 0 || (floor_branch && t.ratio_frame > 3.0) {
            failures.push(format!(
                "{name}: {} boxes, per-frame exceedance {:.2}×",
                t.boxes, t.ratio_frame
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
