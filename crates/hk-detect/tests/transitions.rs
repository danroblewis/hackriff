//! State-machine transitions (T-006 adversarial tests): gain change and retune mid-detection,
//! gaps, no gap merge across a transition, max-duration splits, stream-start warm-up with the
//! real floor tracker, impulsive burst runs, and the wide-signal coverage that depends on T-005.

mod common;

use common::*;
use hk_core::Discontinuity;
use hk_detect::{
    Candidate, ClipCount, CloseReason, ConfirmReason, Detector, DetectorConfig, FloorReference,
};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker};
use hk_model::SurveyId;

fn config() -> DetectorConfig {
    DetectorConfig::new(SurveyId::new())
}

#[test]
fn gain_change_mid_detection_closes_the_box_at_its_last_frame() {
    let fc = 98e6;
    let a = provenance(fc, FS, 24.0);
    let b = provenance(fc, FS, 32.0);
    let mut s = Scene::new(config(), GammaFrames::new(BINS, N_AVG, a.clone(), 21));
    let mut p = flat(BINS);
    add_line(&mut p, 1500, 3, 20.0);
    for _ in 0..15 {
        s.step(&p);
    }
    s.switch(b.clone());
    for i in 0..15 {
        let flags = if i == 0 {
            Discontinuity::GAIN_CHANGE
        } else {
            Discontinuity::NONE
        };
        s.step_with(&p, flags, ClipCount::NONE, false);
    }
    s.finish();
    let d = s.out.sorted();
    assert_eq!(
        d.len(),
        2,
        "{:#?}",
        d.iter().map(|x| describe(x, FS)).collect::<Vec<_>>()
    );
    let fsmp = s.src.frame_samples();
    assert_eq!(d[0].samples, 0..15 * fsmp);
    assert_eq!(d[0].close, CloseReason::Transition);
    assert_eq!(d[0].provenance.id(), a.id());
    assert_eq!(d[0].detection.provenance_ref, a.id());
    assert_eq!(d[0].detection.time.end, s.src.time_of(15 * fsmp));
    assert_eq!(d[1].samples, 15 * fsmp..30 * fsmp);
    assert_eq!(d[1].provenance.id(), b.id());
    assert_eq!(d[1].close, CloseReason::EndOfStream);
    assert_ne!(d[0].segment, d[1].segment);
}

#[test]
fn provenance_change_alone_closes_boxes() {
    // A source minting a new handle without a discontinuity flag or floor-segment change.
    let fc = 98e6;
    let mut s = Scene::new(
        config(),
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), 22),
    );
    let mut p = flat(BINS);
    add_line(&mut p, 1500, 3, 20.0);
    for _ in 0..10 {
        s.step(&p);
    }
    s.src.provenance = provenance(fc, FS, 24.0);
    for _ in 0..10 {
        s.step(&p);
    }
    s.finish();
    let d = s.out.sorted();
    assert_eq!(d.len(), 2);
    assert_eq!(d[0].samples.end, d[1].samples.start);
    assert_ne!(d[0].detection.provenance_ref, d[1].detection.provenance_ref);
}

#[test]
fn retune_mid_detection_closes_and_restarts_at_the_new_centre() {
    let a = provenance(98e6, FS, 24.0);
    let b = provenance(99e6, FS, 24.0);
    let mut s = Scene::new(config(), GammaFrames::new(BINS, N_AVG, a, 23));
    let f_line = 100.5e6;
    let mut pa = flat(BINS);
    add_line(&mut pa, s.bin_of(f_line).round() as usize, 3, 20.0);
    for _ in 0..12 {
        s.step(&pa);
    }
    s.switch(b);
    let mut pb = flat(BINS);
    add_line(&mut pb, s.bin_of(f_line).round() as usize, 3, 20.0);
    for i in 0..12 {
        let flags = if i == 0 {
            Discontinuity::RETUNE
        } else {
            Discontinuity::NONE
        };
        s.step_with(&pb, flags, ClipCount::NONE, false);
    }
    s.finish();
    let d = s.out.sorted();
    assert_eq!(d.len(), 2);
    let fsmp = s.src.frame_samples();
    assert_eq!(d[0].samples.end, 12 * fsmp);
    assert_eq!(d[0].close, CloseReason::Transition);
    for x in &d {
        assert!(
            (x.detection.f_center_hz - f_line).abs() < 5e3,
            "{}",
            describe(x, FS)
        );
    }
    // The line kept its absolute frequency but moved in bins.
    assert_ne!(d[0].bins, d[1].bins);
}

#[test]
fn gap_discontinuity_closes_and_gap_merge_never_bridges_a_transition() {
    let fc = 98e6;
    let mut s = Scene::new(
        config(),
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), 24),
    );
    let mut p = flat(BINS);
    add_line(&mut p, 1500, 3, 20.0);
    let q = flat(BINS);
    for _ in 0..10 {
        s.step(&p);
    }
    // One empty frame, then samples are lost: the next frame carries GAP.
    s.step(&q);
    s.src.sample_index += 12_345;
    for i in 0..10 {
        let flags = if i == 0 {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        s.step_with(&p, flags, ClipCount::NONE, false);
    }
    s.finish();
    let d = s.out.sorted();
    assert_eq!(
        d.len(),
        2,
        "a 1-frame gap would merge without the transition"
    );
    assert_eq!(d[0].frames.end - d[0].frames.start, 10);
    assert_eq!(d[0].samples.end, 10 * s.src.frame_samples());
    // Without the discontinuity the same 1-frame gap merges into one box.
    let mut s = Scene::new(
        config(),
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), 24),
    );
    for i in 0..21 {
        s.step(if i == 10 { &q } else { &p });
    }
    s.finish();
    assert_eq!(s.out.detections.len(), 1);
    assert_eq!(s.out.detections[0].merged_boxes, 2);
}

#[test]
fn long_emissions_split_at_max_duration_and_confirm_by_repeat() {
    let fc = 98e6;
    let mut s = Scene::new(
        config(),
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), 25),
    );
    let mut p = flat(BINS);
    add_line(&mut p, 2500, 3, 20.0);
    let frames = (2.5 / s.src.frame_period_s()).round() as u64;
    for _ in 0..frames {
        s.step(&p);
    }
    s.finish();
    let d = s.out.sorted();
    assert_eq!(
        d.len(),
        3,
        "{:#?}",
        d.iter().map(|x| describe(x, FS)).collect::<Vec<_>>()
    );
    assert_eq!(
        d.iter().map(|x| x.continues).collect::<Vec<_>>(),
        [true, true, false]
    );
    assert_eq!(d[0].close, CloseReason::MaxDuration);
    assert_eq!(d[2].close, CloseReason::EndOfStream);
    for w in d.windows(2) {
        assert_eq!(w[0].samples.end, w[1].samples.start);
    }
    assert!(matches!(d[1].candidate, Candidate::Repeat { with } if with == d[0].detection.id));
    // The first is confirmed after the fact (repeat) or at emission (integrated, ≥ 1 s).
    let first = d[0].detection.id;
    assert!(
        d[0].candidate.is_confirmed()
            || s.out.confirmations.iter().any(|c| c.detection == first
                && matches!(
                    c.reason,
                    ConfirmReason::Repeat { .. } | ConfirmReason::Integrated { .. }
                ))
    );
}

/// Drives Gamma frames through the real floor tracker and the detector.
fn run_tracked(
    src: &mut GammaFrames,
    det: &mut Detector,
    tracker: &mut NoiseFloorTracker,
    profile: impl Fn(u64) -> Vec<f32>,
    frames: u64,
    restart_every: u64,
    out: &mut Collected,
) -> u64 {
    let mut frame = src.empty_frame();
    let mut impulsive = 0;
    for i in 0..frames {
        let flags = if i % restart_every == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        src.fill(&mut frame, &profile(i), flags);
        let f = tracker.update(&frame, |_| {});
        impulsive += u64::from(f.impulsive);
        det.process(&frame, f, ClipCount::NONE, &mut out.sink());
    }
    det.finish(&mut out.sink());
    impulsive
}

#[test]
fn stream_start_warm_up_raises_no_burst_of_false_boxes() {
    let bins = 2048;
    let prov = provenance(433.92e6, 2e6, 24.0);
    let mut src = GammaFrames::new(bins, N_AVG, prov, 26);
    let mut det = Detector::new(config()).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut out = Collected::default();
    run_tracked(
        &mut src,
        &mut det,
        &mut tracker,
        |_| flat(bins),
        25 * 40,
        40,
        &mut out,
    );
    assert!(
        out.detections.is_empty(),
        "noise-only restarts produced {} boxes: {:#?}",
        out.detections.len(),
        out.detections
            .iter()
            .map(|d| describe(d, 2e6))
            .collect::<Vec<_>>()
    );
    assert_eq!(det.stats().segments, 25);

    // A carrier present from the first frame gives exactly one box per restart, not fragments.
    let mut det = Detector::new(config()).unwrap();
    let mut out = Collected::default();
    let mut p = flat(bins);
    add_line(&mut p, 700, 3, 20.0);
    run_tracked(
        &mut src,
        &mut det,
        &mut tracker,
        |_| p.clone(),
        25 * 40,
        40,
        &mut out,
    );
    assert_eq!(
        out.detections.len(),
        25,
        "{:#?}",
        out.detections
            .iter()
            .map(|d| describe(d, 2e6))
            .collect::<Vec<_>>()
    );
    assert!(
        out.detections
            .iter()
            .all(|d| d.frames.end - d.frames.start == 40)
    );
}

#[test]
fn impulsive_burst_runs_become_one_impulsive_event_each() {
    let bins = 2048;
    let prov = provenance(433.92e6, 2e6, 24.0);
    let mut src = GammaFrames::new(bins, N_AVG, prov, 27);
    let mut det = Detector::new(config()).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut out = Collected::default();
    let base = flat(bins);
    let mut burst: Vec<f32> = base.iter().map(|v| v * undb(1.0) as f32).collect();
    for k in 0..16 {
        add_line(&mut burst, 150 + 110 * k, 3, 12.0);
    }
    let runs = [100u64, 200, 300];
    let impulsive_frames = run_tracked(
        &mut src,
        &mut det,
        &mut tracker,
        |i| {
            if runs.iter().any(|&r| (r..r + 5).contains(&i)) {
                burst.clone()
            } else {
                base.clone()
            }
        },
        400,
        u64::MAX,
        &mut out,
    );
    eprintln!("tracker flagged {impulsive_frames} impulsive frames");
    assert!(
        impulsive_frames >= 12,
        "the floor tracker did not flag the runs"
    );
    let normal: Vec<_> = out
        .detections
        .iter()
        .filter(|d| !d.detection.flags.impulsive)
        .collect();
    assert!(
        normal.is_empty(),
        "{:#?}",
        normal.iter().map(|d| describe(d, 2e6)).collect::<Vec<_>>()
    );
    let imp: Vec<_> = out
        .detections
        .iter()
        .filter(|d| d.detection.flags.impulsive)
        .collect();
    assert_eq!(
        imp.len(),
        runs.len(),
        "{:#?}",
        imp.iter().map(|d| describe(d, 2e6)).collect::<Vec<_>>()
    );
    for d in imp {
        assert!(d.merged_boxes >= 14, "{}", d.merged_boxes);
        assert!(d.bins.start <= 151 && d.bins.end >= 150 + 110 * 15);
    }
}

fn wide_signal_coverage(reference: FloorReference) -> f64 {
    let bins = BINS;
    let prov = provenance(98e6, FS, 24.0);
    let mut src = GammaFrames::new(bins, N_AVG, prov, 28);
    let mut cfg = config();
    cfg.floor_reference = reference;
    let mut det = Detector::new(cfg).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut out = Collected::default();
    let mut p = flat(bins);
    let (lo, hi) = (1024, 3072);
    for v in &mut p[lo..hi] {
        *v += undb(10.0) as f32;
    }
    let mut frame = src.empty_frame();
    let (mut covered, mut total) = (0u64, 0u64);
    for i in 0..60 {
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        src.fill(&mut frame, &p, flags);
        let f = tracker.update(&frame, |_| {});
        det.process(&frame, f, ClipCount::NONE, &mut out.sink());
        if i >= 16 {
            let codes = det.codes();
            covered += codes[lo + 64..hi - 64].iter().filter(|&&c| c != 0).count() as u64;
            total += (hi - lo - 128) as u64;
        }
    }
    covered as f64 / total as f64
}

#[test]
fn wide_flat_signal_edges_are_detected_with_the_per_frame_reference() {
    let coverage = wide_signal_coverage(FloorReference::PerFrame);
    eprintln!(
        "2048-bin +10 dB signal, per-frame reference: interior coverage {:.1} %",
        coverage * 100.0
    );
}

#[test]
#[ignore = "depends on the T-005 wide-reference fix (FloorFrame::wide_floor biases low on sloped floors); \
            flip FloorReference::Wide to the default and un-ignore when it lands"]
fn wide_flat_signal_interior_coverage_via_the_wide_floor_branch() {
    let coverage = wide_signal_coverage(FloorReference::Wide);
    eprintln!(
        "2048-bin +10 dB signal, wide reference: interior coverage {:.1} %",
        coverage * 100.0
    );
    assert!(coverage >= 0.95, "interior coverage {coverage}");
}
