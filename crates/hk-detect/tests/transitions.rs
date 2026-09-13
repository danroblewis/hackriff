//! State-machine transitions (T-006 adversarial tests): gain change and retune mid-detection,
//! gaps, no gap merge across a transition, max-duration splits, stream-start warm-up with the
//! real floor tracker, impulsive burst runs, wide-signal coverage (T-005 wide reference) and flat
//! signals next to a notch edge (T-028).

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

/// Two 20 dB carriers at bins 1000 and 1400 for `seconds`, with bridge frames (+12 dB over bins
/// 990–1410) at `bridges` (frame indices), optionally flagged impulsive. Returns the records.
fn two_carriers(
    seconds: f64,
    bridges: &[u64],
    impulsive: bool,
) -> (Vec<hk_detect::DetectionRecord>, f64) {
    let mut s = Scene::new(
        config(),
        GammaFrames::new(BINS, N_AVG, provenance(98e6, FS, 24.0), 29),
    );
    let mut base = flat(BINS);
    add_line(&mut base, 1000, 3, 20.0);
    add_line(&mut base, 1400, 3, 20.0);
    let mut bridge = base.clone();
    for v in &mut bridge[990..1411] {
        *v += undb(12.0) as f32;
    }
    let frames = (seconds / s.src.frame_period_s()).round() as u64;
    for t in 0..frames {
        let b = bridges.contains(&t);
        s.step_with(
            if b { &bridge } else { &base },
            Discontinuity::NONE,
            ClipCount::NONE,
            b && impulsive,
        );
    }
    s.finish();
    let per_box = (1.0 / s.src.frame_period_s()).ceil();
    (s.out.detections, per_box)
}

fn describe_all(d: &[hk_detect::DetectionRecord]) -> Vec<String> {
    d.iter().map(|x| describe(x, FS)).collect()
}

#[test]
fn a_bridging_frame_fuses_carriers_for_at_most_one_box() {
    let narrow = |d: &hk_detect::DetectionRecord| d.f_hi_hz - d.f_lo_hz < 50e3;
    let (plain, _) = two_carriers(3.0, &[], false);
    assert_eq!(plain.len(), 6, "{:#?}", describe_all(&plain));
    assert!(plain.iter().all(narrow));
    // One non-impulsive bridge at frame 100: the first 1 s box is fused, the rest are not.
    let (bridged, _) = two_carriers(3.0, &[100], false);
    let wide: Vec<_> = bridged.iter().filter(|d| !narrow(d)).collect();
    eprintln!("bridged: {:#?}", describe_all(&bridged));
    assert_eq!(wide.len(), 1, "{:#?}", describe_all(&bridged));
    assert!(wide[0].frames.contains(&101));
    let after: Vec<_> = bridged
        .iter()
        .filter(|d| d.frames.start >= wide[0].frames.end)
        .collect();
    assert_eq!(after.len(), 4, "{:#?}", describe_all(&bridged));
    assert!(after.iter().all(|d| narrow(d)));
    for bin in [1000, 1400] {
        assert_eq!(after.iter().filter(|d| d.bins.contains(&bin)).count(), 2);
    }
}

#[test]
fn an_impulsive_frame_touching_two_carriers_never_fuses_them() {
    let (records, _) = two_carriers(3.0, &[100], true);
    eprintln!("impulsive bridge: {:#?}", describe_all(&records));
    let normal: Vec<_> = records
        .iter()
        .filter(|d| !d.detection.flags.impulsive)
        .collect();
    assert_eq!(normal.len(), 6, "{:#?}", describe_all(&records));
    assert!(normal.iter().all(|d| d.f_hi_hz - d.f_lo_hz < 50e3));
}

#[test]
fn occasional_bridging_frames_never_fuse_records_permanently() {
    // 10 s, bridges at 1.25, 3.75, 6.25 and 8.75 s: only the four 1 s boxes that contain a bridge
    // are fused; every other box holds the two carriers separately.
    let period = 2.048e-3;
    let bridges: Vec<u64> = [1.25, 3.75, 6.25, 8.75]
        .iter()
        .map(|s| (s / period) as u64)
        .collect();
    let (records, per_box) = two_carriers(10.0, &bridges, false);
    let wide: Vec<_> = records
        .iter()
        .filter(|d| d.f_hi_hz - d.f_lo_hz >= 50e3)
        .collect();
    let narrow = records.len() - wide.len();
    eprintln!(
        "10 s with 4 bridges: {} records, {} fused, {narrow} separate ({per_box} frames per box)",
        records.len(),
        wide.len()
    );
    assert_eq!(wide.len(), 4, "{:#?}", describe_all(&records));
    for w in &wide {
        assert!(
            bridges.iter().any(|&b| w.frames.contains(&(b + 1))),
            "{}",
            describe(w, FS)
        );
    }
    assert_eq!(narrow, 12, "{:#?}", describe_all(&records));
    let last = records.iter().max_by_key(|d| d.frames.end).unwrap();
    assert!(
        last.f_hi_hz - last.f_lo_hz < 50e3,
        "the stream ends unfused"
    );
}

fn wide_signal_coverage(reference: FloorReference) -> (f64, f64) {
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
    let (mut covered, mut total, mut edge_frames, mut frames) = (0u64, 0u64, 0u64, 0u64);
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
            // Both signal edges (±8 bins) have a detected cell in this frame.
            let edge = |c: usize| codes[c - 8..c + 8].iter().any(|&x| x != 0);
            edge_frames += u64::from(edge(lo) && edge(hi));
            frames += 1;
        }
    }
    (
        covered as f64 / total as f64,
        edge_frames as f64 / frames as f64,
    )
}

#[test]
fn wide_flat_signal_edges_are_detected_with_the_per_frame_reference() {
    // The per-frame floor reads the signal inside a flat signal wider than a block, and the
    // floor-step guard switches the floor branch off near its edges, so the OS branch finds the
    // edges and little of the interior (T-005's wide reference is the fix for the interior).
    let (coverage, edges) = wide_signal_coverage(FloorReference::PerFrame);
    eprintln!(
        "2048-bin +10 dB signal, per-frame reference: interior coverage {:.1} %, both edges detected in {:.0} % of frames",
        coverage * 100.0,
        edges * 100.0
    );
    assert!(edges >= 0.9, "edges detected in {edges} of frames");
    assert!(coverage >= 0.005, "interior coverage {coverage}");
}

/// A flat `width`-bin signal `snr_db` above the floor starting `gap` bins above a −20 dB notch
/// (bins 1600..2000, or none): interior coverage (cells detected, 10 % trimmed each side) over
/// frames 64..600 through the real tracker, and records overlapping the signal.
fn notch_neighbour_coverage(
    width: usize,
    snr_db: f64,
    with_notch: bool,
    reference: FloorReference,
) -> (f64, usize) {
    let prov = provenance(98e6, FS, 24.0);
    let mut src = GammaFrames::new(BINS, N_AVG, prov, 29);
    let mut cfg = config();
    cfg.floor_reference = reference;
    let mut det = Detector::new(cfg).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut out = Collected::default();
    let mut p = flat(BINS);
    let (n_lo, n_hi, gap) = (1600, 2000, 60);
    if with_notch {
        for v in &mut p[n_lo..n_hi] {
            *v = undb(-20.0) as f32;
        }
    }
    let (lo, hi) = (n_hi + gap, n_hi + gap + width);
    for v in &mut p[lo..hi] {
        *v += undb(snr_db) as f32;
    }
    let trim = width / 10;
    let mut frame = src.empty_frame();
    let (mut covered, mut total) = (0u64, 0u64);
    for i in 0..600 {
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        src.fill(&mut frame, &p, flags);
        let f = tracker.update(&frame, |_| {});
        det.process(&frame, f, ClipCount::NONE, &mut out.sink());
        if i >= 64 {
            let codes = det.codes();
            covered += codes[lo + trim..hi - trim]
                .iter()
                .filter(|&&c| c != 0)
                .count() as u64;
            total += (hi - lo - 2 * trim) as u64;
        }
    }
    det.finish(&mut out.sink());
    let records = out
        .detections
        .iter()
        .filter(|d| d.bins.start < hi && lo < d.bins.end)
        .count();
    (covered as f64 / total as f64, records)
}

#[test]
fn flat_signals_next_to_a_notch_edge_keep_interior_coverage() {
    // T-006 re-probe: OS-only inside the guarded zone missed flat signals wider than ~16 bins
    // (41-bin +15 dB: 0 records; 300-bin: 84.5 % → 0.2 %). Guarded bins now run the floor branch
    // against the shape-normalised wide reference once the shape explains the step.
    let mut failures = Vec::new();
    for (width, snr) in [(41, 15.0), (300, 10.0)] {
        for reference in [FloorReference::PerFrame, FloorReference::Wide] {
            let (open, open_records) = notch_neighbour_coverage(width, snr, false, reference);
            let (near, records) = notch_neighbour_coverage(width, snr, true, reference);
            eprintln!(
                "{width}-bin +{snr} dB, {reference:?}: interior coverage {:.1} % next to a -20 dB notch \
                 ({records} records), {:.1} % without ({open_records} records)",
                near * 100.0,
                open * 100.0
            );
            if near < 0.8 || records == 0 {
                failures.push(format!(
                    "{width}-bin {reference:?}: {:.1} %, {records} records",
                    near * 100.0
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn wide_flat_signal_interior_coverage_via_the_wide_floor_branch() {
    // T-005's slope-robust wide reference (re-enabled in T-028; not the default, see
    // `FloorReference::Wide`).
    let (coverage, _) = wide_signal_coverage(FloorReference::Wide);
    eprintln!(
        "2048-bin +10 dB signal, wide reference: interior coverage {:.1} %",
        coverage * 100.0
    );
    assert!(coverage >= 0.95, "interior coverage {coverage}");
}
