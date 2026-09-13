//! T-005 detection references on shaped floors (the T-006 floor branch): the per-frame floor and
//! the wide-signal reference ([`FloorKind::Wide`]) keep the floor-branch per-cell false-alarm
//! rate within 1.5× design on tilted floors (0–12 dB across the span), HackRF-like baseband
//! roll-offs, notches and the real urban FM capture, while a 2048-bin +10 dB signal's interior
//! stays ≥ 95 % covered. Re-review finding 1 against 854614c (the old sliding minimum read up to
//! 6000× design). SPACE-050 (floor survey) / AWARE-042 (detection over real captures).

mod common;
mod floor_common;

use common::*;
use floor_common::*;
use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{
    FloorConfig, FloorKind, FloorThreshold, NoiseFloorTracker, effective_averages,
};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_model::{SampleTime, Timestamp};
use num_complex::Complex;

const BINS: usize = 4096;
const PFAS: [f64; 2] = [1e-3, 1e-4];
/// Frames before counting: the response shape learns with τ = 1 s (≈ 49 frames here).
const LEARN: usize = 100;
const MEASURE: usize = 300;

/// Floor-branch exceedance over design for `[Frame, Wide] × PFAS` on a Gamma(10) floor shaped by
/// `profile`, after the shape has learned.
fn pfa_ratios(name: &str, profile: &[f32], seed: u64) -> [[f64; 2]; 2] {
    // 2 Msps: a 20.5 ms frame, so shape learning takes few frames.
    let mut src = GammaFrames::new(BINS, 10, provenance(100e6, 2e6), seed);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let thresholds = PFAS.map(|p| FloorThreshold::single(10.0, p).on as f32);
    let mut hits = [[0u64; 2]; 2];
    let mut frame = src.empty_frame();
    for k in 0..LEARN + MEASURE {
        src.fill(&mut frame, profile, Discontinuity::NONE);
        let f = tracker.update(&frame, |_| {});
        if k < LEARN {
            continue;
        }
        for (r, kind) in [FloorKind::Frame, FloorKind::Wide].into_iter().enumerate() {
            for (&p, &fl) in frame.spectrum.psd.iter().zip(f.trace(kind)) {
                for (h, &t) in hits[r].iter_mut().zip(&thresholds) {
                    *h += u64::from(p > t * fl);
                }
            }
        }
    }
    let cells = (MEASURE * BINS) as f64;
    let ratios = hits.map(|h| [0, 1].map(|j| h[j] as f64 / cells / PFAS[j]));
    let f = tracker.last().unwrap();
    let shaped = f.shape.iter().filter(|&&s| s < 1.0).count();
    eprintln!(
        "{name}: floor-branch Pfa / design at 1e-3 | 1e-4: frame {:.2} | {:.2}, wide {:.2} | {:.2} ({shaped} shaped bins)",
        ratios[0][0], ratios[0][1], ratios[1][0], ratios[1][1]
    );
    for (r, which) in ["frame", "wide"].into_iter().enumerate() {
        for (j, pfa) in PFAS.iter().enumerate() {
            let x = ratios[r][j];
            assert!(
                (0.5..=1.5).contains(&x),
                "{name}: {which} floor-branch Pfa at {pfa:e} is {x:.2}× design"
            );
        }
    }
    ratios
}

fn tilt(db: f64) -> Vec<f32> {
    (0..BINS)
        .map(|i| 10f64.powf(db * (i as f64 / (BINS - 1) as f64 - 0.5) / 10.0) as f32)
        .collect()
}

/// Butterworth baseband response of `order` with cut-off at `cutoff`·fs, over a −25 dB
/// pedestal (the ADC's own noise past the filter).
fn rolloff(cutoff: f64, order: i32) -> Vec<f32> {
    (0..BINS)
        .map(|i| {
            let f = (i as f64 - BINS as f64 / 2.0).abs() / BINS as f64;
            (1.0 / (1.0 + (f / cutoff).powi(2 * order)) + 10f64.powf(-2.5)) as f32
        })
        .collect()
}

/// A −20 dB notch over bins `a..b` with raised-cosine skirts of `skirt` bins.
fn notch(a: usize, b: usize, skirt: usize) -> Vec<f32> {
    (0..BINS)
        .map(|i| {
            let x = if (a..b).contains(&i) {
                1.0
            } else if i < a && i + skirt > a {
                1.0 - (a - i) as f64 / skirt as f64
            } else if i >= b && i < b + skirt {
                1.0 - (i - b + 1) as f64 / skirt as f64
            } else {
                0.0
            };
            let w = 0.5 - 0.5 * (std::f64::consts::PI * x).cos();
            10f64.powf(-2.0 * w) as f32
        })
        .collect()
}

#[test]
fn tilted_floors_keep_the_design_false_alarm_rate() {
    for (k, db) in [0.0, 3.0, 6.0, 9.0, 12.0].into_iter().enumerate() {
        pfa_ratios(&format!("tilt {db} dB"), &tilt(db), 100 + k as u64);
    }
}

#[test]
fn hackrf_baseband_rolloff_keeps_the_design_false_alarm_rate() {
    // 15 MHz filter at 20 Msps (8th order), and a steeper 16th-order skirt at 0.4 fs.
    pfa_ratios("roll-off 8th order at 0.375 fs", &rolloff(0.375, 8), 200);
    pfa_ratios("roll-off 16th order at 0.40 fs", &rolloff(0.40, 16), 201);
}

#[test]
fn notches_keep_the_design_false_alarm_rate() {
    pfa_ratios("-20 dB notch, 128-bin skirts", &notch(1800, 2300, 128), 300);
    pfa_ratios("-20 dB notch, sharp", &notch(1800, 2300, 1), 301);
}

#[test]
fn wide_signal_interior_stays_covered_while_the_shape_learns() {
    let mut src = GammaFrames::new(BINS, 10, provenance(100e6, 2e6), 12);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut profile = vec![1.0f32; BINS];
    profile[1024..3072].fill(11.0); // +10 dB over the floor, present from the first frame
    let t = FloorThreshold::s4(10.0);
    let (mut on, mut guard, mut total) = (0usize, 0usize, 0usize);
    let mut worst_frame = 1.0f64;
    let mut frame = src.empty_frame();
    for _ in 0..200 {
        src.fill(&mut frame, &profile, Discontinuity::NONE);
        let f = tracker.update(&frame, |_| {});
        let mut frame_on = 0;
        for i in 1024..3072 {
            let (p, w) = (frame.spectrum.psd[i], f.wide_floor[i]);
            on += usize::from(p > t.level_on(w));
            guard += usize::from(p > t.guard_level(w));
            frame_on += usize::from(p > t.level_on(w) && p > t.guard_level(w));
            total += 1;
        }
        worst_frame = worst_frame.min(frame_on as f64 / 2048.0);
    }
    let pct = |n: usize| 100.0 * n as f64 / total as f64;
    eprintln!(
        "2048-bin +10 dB signal over 200 frames: floor branch {:.1} %, guard {:.1} %, worst frame {:.1} %",
        pct(on),
        pct(guard),
        100.0 * worst_frame
    );
    assert!(pct(on) >= 95.0 && pct(guard) >= 95.0 && worst_frame >= 0.9);
}

#[test]
fn real_urban_capture_wide_reference_matches_the_integrated_floor() {
    let meta = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hackrf/2026-09-13/urban_98M_20M_l24g20a0_t1p0_0p6s.sigmf-meta"
    );
    let fx = hk_e2e::Fixture::load(meta).expect("fixture metadata");
    let n = 4_000_000; // 0.2 s
    let samples = match fx.samples_range(0, n) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SKIP (fixture data not fetched: {e})");
            return;
        }
    };
    let ci8: Vec<Complex<i8>> = samples
        .iter()
        .map(|s| Complex::new((s.re * 127.0).round() as i8, (s.im * 127.0).round() as i8))
        .collect();
    let prov = ProvenanceHandle::new(fx.meta.global.provenance.clone().expect("provenance"));
    let header = BlockHeader {
        time: SampleTime {
            sample_index: 0,
            host_time: Timestamp::UNIX_EPOCH,
        },
        provenance: prov,
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
    };
    let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(BINS), 10)).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let (mut psds, mut frames_ref, mut wides) = (Vec::new(), Vec::new(), Vec::new());
    let mut n_eff = 0.0;
    stft.push(InputInfo::from(&header), &ci8, |frame| {
        n_eff = effective_averages(&frame.spectrum.resolution);
        let f = tracker.update(frame, |_| {});
        psds.push(frame.spectrum.psd.clone());
        frames_ref.push(f.floor.clone());
        wides.push(f.wide_floor.clone());
    });
    let nf = psds.len();
    assert!(nf >= 150);
    // The integrated (time-mean) spectrum is the per-bin truth where it sits on the floor:
    // "quiet" bins are within 0.5 dB of the time-median per-frame floor.
    let mut mean = vec![0f64; BINS];
    for p in &psds {
        for (m, &x) in mean.iter_mut().zip(p) {
            *m += f64::from(x) / nf as f64;
        }
    }
    let mut quiet = vec![false; BINS];
    let mut col = vec![0f32; nf];
    for i in 0..BINS {
        for (c, r) in col.iter_mut().zip(&frames_ref) {
            *c = r[i];
        }
        col.sort_by(f32::total_cmp);
        quiet[i] = mean[i] <= f64::from(col[nf / 2]) * 1.122;
    }
    let thresholds = PFAS.map(|p| FloorThreshold::single(n_eff, p).on as f32);
    let (mut truth, mut frame_hits, mut wide_hits) = ([0u64; 2], [0u64; 2], [0u64; 2]);
    let mut level = Vec::new();
    for k in 0..nf {
        for i in (0..BINS).filter(|&i| quiet[i]) {
            let p = psds[k][i];
            for j in 0..2 {
                truth[j] += u64::from(p > thresholds[j] * mean[i] as f32);
                frame_hits[j] += u64::from(p > thresholds[j] * frames_ref[k][i]);
                wide_hits[j] += u64::from(p > thresholds[j] * wides[k][i]);
            }
            if k % 4 == 0 {
                level.push(db(f64::from(wides[k][i]) / mean[i]));
            }
        }
    }
    level.sort_by(f64::total_cmp);
    let low = level.iter().filter(|&&x| x < -1.0).count() as f64 / level.len() as f64;
    let ratio = |h: [u64; 2]| [h[0] as f64 / truth[0] as f64, h[1] as f64 / truth[1] as f64];
    let (fr, wr) = (ratio(frame_hits), ratio(wide_hits));
    let quiet_n = quiet.iter().filter(|&&q| q).count();
    eprintln!(
        "urban 98 MHz, {nf} frames, {quiet_n} quiet bins: exceedance vs the integrated-floor reference at 1e-3 | 1e-4: frame {:.2} | {:.2}, wide {:.2} | {:.2}; wide level vs integrated floor p5 {:+.2} dB, median {:+.2} dB, {:.1} % below -1 dB",
        fr[0],
        fr[1],
        wr[0],
        wr[1],
        level[level.len() / 20],
        level[level.len() / 2],
        100.0 * low
    );
    assert!(
        wr[0] <= 1.5 && wr[1] <= 1.5,
        "wide reference too low on real noise"
    );
    assert!(level[level.len() / 20] > -1.0 && low < 0.05);
}

#[test]
fn notch_removal_and_insertion_recover_within_one_second() {
    let flat = vec![1.0f32; BINS];
    let t3 = FloorThreshold::single(10.0, 1e-3).on as f32;
    for (k, (name, removed)) in [
        ("-20 dB notch removed at 5 s", true),
        ("sharp -20 dB notch inserted at 5 s", false),
    ]
    .into_iter()
    .enumerate()
    {
        let mut src = GammaFrames::new(BINS, 10, provenance(100e6, 2e6), 400 + k as u64);
        let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
        let n = if removed {
            notch(1800, 2300, 128)
        } else {
            notch(1800, 2300, 1)
        };
        let period = src.frame_period_s();
        let mut frame = src.empty_frame();
        // [window [5, 6) s, window [6, 9) s] × [notch region, whole band]: (hits, cells).
        let mut win = [[(0u64, 0u64); 2]; 2];
        for k in 0..(9.0 / period) as usize {
            let t = k as f64 * period;
            let profile = if (t < 5.0) == removed { &n } else { &flat };
            src.fill(&mut frame, profile, Discontinuity::NONE);
            let f = tracker.update(&frame, |_| {});
            if t < 5.0 {
                continue;
            }
            let w = usize::from(t >= 6.0);
            for (r, range) in [1672..2428usize, 0..BINS].into_iter().enumerate() {
                for i in range {
                    win[w][r].0 += u64::from(frame.spectrum.psd[i] > t3 * f.wide_floor[i]);
                    win[w][r].1 += 1;
                }
            }
        }
        let ratio = |(h, c): (u64, u64)| h as f64 / c as f64 / 1e-3;
        eprintln!(
            "{name}: wide floor-branch Pfa / design at 1e-3: first second notch {:.1}, band {:.1}; 1-4 s later notch {:.2}, band {:.2}",
            ratio(win[0][0]),
            ratio(win[0][1]),
            ratio(win[1][0]),
            ratio(win[1][1])
        );
        assert!(
            ratio(win[1][0]) <= 1.5 && ratio(win[1][1]) <= 1.5,
            "{name}: the shape has not caught up within 1 s"
        );
    }
}
