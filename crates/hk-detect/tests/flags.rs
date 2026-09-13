//! Flag rules on known-floor Gamma frames (S4 rules 1, 2, 3, 4, 5, 8 and the flag table):
//! reference harmonic, DC, spur map, comb, IQ image, clipping, impulsive merge, edge, marginal.

mod common;

use common::*;
use hk_core::Discontinuity;
use hk_detect::{ClipCount, DetectorConfig, Rules};
use hk_model::detection::SpurReason;
use hk_model::{FreqRange, SpurMask, SpurMaskId, SpurRule, SurveyId, Timestamp};

fn scene_at(fc: f64, seed: u64) -> Scene {
    Scene::new(
        DetectorConfig::new(SurveyId::new()),
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), seed),
    )
}

fn near(s: &Scene, f_hz: f64, tol_hz: f64) -> Vec<&hk_detect::DetectionRecord> {
    s.out
        .detections
        .iter()
        .filter(|d| (d.detection.f_center_hz - f_hz).abs() <= tol_hz)
        .collect()
}

#[test]
fn ref_harmonic_spur_at_n_times_10_mhz_is_flagged_and_an_off_raster_line_is_not() {
    let fc = 98e6;
    let mut s = scene_at(fc, 1);
    let mut p = flat(BINS);
    add_line(&mut p, s.bin_of(100e6).round() as usize, 3, 20.0);
    add_line(&mut p, s.bin_of(100.3e6).round() as usize, 3, 20.0);
    for _ in 0..20 {
        s.step(&p);
    }
    s.finish();
    let spur = near(&s, 100e6, 10e3);
    assert!(!spur.is_empty(), "100 MHz line not detected");
    for d in spur {
        assert_eq!(
            d.detection.flags.spur_reason,
            Some(SpurReason::RefHarmonic),
            "{}",
            describe(d, FS)
        );
        assert!(d.detection.flags.spur_candidate);
        assert_eq!(d.spur_harmonic_hz, Some(100e6));
    }
    let other = near(&s, 100.3e6, 10e3);
    assert!(!other.is_empty());
    assert!(other.iter().all(|d| !d.detection.flags.spur_candidate));
}

#[test]
fn dc_line_is_flagged_dc() {
    let fc = 433.92e6;
    let mut s = scene_at(fc, 2);
    let mut p = flat(BINS);
    add_line(&mut p, BINS / 2, 3, 25.0);
    for _ in 0..12 {
        s.step(&p);
    }
    s.finish();
    let dc = near(&s, fc, 15e3);
    assert!(!dc.is_empty());
    for d in dc {
        assert_eq!(
            d.detection.flags.spur_reason,
            Some(SpurReason::Dc),
            "{}",
            describe(d, FS)
        );
    }
}

#[test]
fn spur_map_rule_names_the_mask() {
    let fc = 98e6;
    let mask = SpurMask {
        id: SpurMaskId::new(),
        supersedes: None,
        device_id: "synthetic:hk-detect-test".into(),
        measured_at: Timestamp::UNIX_EPOCH,
        rules: vec![SpurRule::Spur {
            freq: FreqRange::centered(96.123e6, 4e3),
            level_dbfs: -80.0,
            gain_db: None,
        }],
    };
    let mut config = DetectorConfig::new(SurveyId::new());
    config.spur_mask = Some(mask.clone());
    let mut s = Scene::new(
        config,
        GammaFrames::new(BINS, N_AVG, provenance(fc, FS, 24.0), 3),
    );
    let mut p = flat(BINS);
    add_line(&mut p, s.bin_of(96.123e6).round() as usize, 3, 18.0);
    for _ in 0..10 {
        s.step(&p);
    }
    s.finish();
    let hits = near(&s, 96.123e6, 10e3);
    assert!(!hits.is_empty());
    for d in hits {
        assert_eq!(
            d.detection.flags.spur_reason,
            Some(SpurReason::SpurMap { mask: mask.id })
        );
    }
}

#[test]
fn comb_of_narrow_lines_is_flagged_from_the_integrated_spectrum() {
    let fc = 98e6;
    let mut s = scene_at(fc, 4);
    let spacing = 209_478.6;
    let comb: Vec<f64> = (0..14).map(|k| 90.2841e6 + k as f64 * spacing).collect();
    let others = [95.123e6, 102.345e6];
    let mut p = flat(BINS);
    for &f in comb.iter().chain(&others) {
        add_fractional_line(&mut p, s.bin_of(f), 12.0);
    }
    // 1.4 s: four 0.25 s blocks complete, boxes split at 1 s and close at the end.
    let frames = (1.4 / s.src.frame_period_s()) as usize;
    for _ in 0..frames {
        s.step(&p);
    }
    s.finish();
    let eval = s
        .out
        .evaluations
        .iter()
        .rev()
        .find(|e| e.comb.flagged)
        .expect("a flagged comb evaluation");
    let m = (spacing / eval.comb.spacing_hz).round();
    assert!(
        (1.0..=2.0).contains(&m) && (eval.comb.spacing_hz * m - spacing).abs() < 300.0,
        "spacing {} Hz",
        eval.comb.spacing_hz
    );
    for &f in &comb {
        let hits = near(&s, f, 5e3);
        assert!(!hits.is_empty(), "comb line {f} not detected");
        for d in hits {
            assert_eq!(
                d.detection.flags.spur_reason,
                Some(SpurReason::Comb),
                "{}",
                describe(d, FS)
            );
        }
    }
    for &f in &others {
        let hits = near(&s, f, 5e3);
        assert!(!hits.is_empty());
        assert!(
            hits.iter().all(|d| !d.detection.flags.spur_candidate),
            "{f}"
        );
    }
    eprintln!(
        "comb: {} members, spacing {:.1} Hz, chance {}",
        eval.comb.members, eval.comb.spacing_hz, eval.comb.chance
    );
}

#[test]
fn image_mirror_of_a_20_db_stronger_emitter_is_flagged() {
    let fc = 98e6;
    let mut s = scene_at(fc, 5);
    let shape = [0.2f32, 0.5, 1.0, 0.7, 0.4, 0.25, 0.1];
    let strong_start = 2662 - 3;
    let mut p = flat(BINS);
    for (j, &w) in shape.iter().enumerate() {
        p[strong_start + j] += (undb(40.0) as f32) * w;
        // The image of bin b is 4096 − b, 25 dB weaker.
        p[BINS - (strong_start + j)] += (undb(15.0) as f32) * w;
    }
    add_line(&mut p, 1300, 5, 15.0);
    for _ in 0..15 {
        s.step(&p);
    }
    s.finish();
    let weak = s
        .out
        .detections
        .iter()
        .filter(|d| d.bins.start <= 1437 && 1431 <= d.bins.end)
        .collect::<Vec<_>>();
    assert!(!weak.is_empty());
    for d in &weak {
        assert!(d.detection.flags.image_candidate, "{}", describe(d, FS));
        let ev = d.image.expect("evidence");
        assert!(ev.rejection_db >= 20.0, "{ev:?}");
        assert!((ev.source_hz - (2.0 * fc - d.detection.f_center_hz)).abs() < 1.0);
        assert!(ev.shape_correlation.is_some_and(|c| c > 0.5), "{ev:?}");
    }
    let strong = s
        .out
        .detections
        .iter()
        .filter(|d| d.bins.start <= 2662 && 2662 < d.bins.end);
    assert!(strong.clone().count() > 0);
    assert!(
        strong
            .into_iter()
            .all(|d| !d.detection.flags.image_candidate)
    );
    let lonely = near(
        &s,
        s.src.provenance.tune.center_hz + (1300.0 - 2048.0) * FS / BINS as f64,
        10e3,
    );
    assert!(!lonely.is_empty() && lonely.iter().all(|d| !d.detection.flags.image_candidate));
}

#[test]
fn clipped_frames_set_clipped_and_count_samples_over_the_threshold_only() {
    let fc = 98e6;
    let samples = u64::from(N_AVG) * BINS as u64;
    let run = |clips: &[(usize, u64)], overload: bool| {
        let prov = provenance_full(fc, FS, 24.0, 15e6, overload, false);
        let mut s = Scene::new(
            DetectorConfig::new(SurveyId::new()),
            GammaFrames::new(BINS, N_AVG, prov, 6),
        );
        let mut p = flat(BINS);
        add_line(&mut p, 1500, 3, 20.0);
        for i in 0..20 {
            let c = clips.iter().find(|(f, _)| *f == i).map_or(0, |&(_, c)| c);
            s.step_with(&p, Discontinuity::NONE, ClipCount::new(c, samples), false);
        }
        s.finish();
        assert_eq!(s.out.detections.len(), 1, "{:#?}", s.out.detections);
        s.out.detections.remove(0)
    };
    // 500 of 40 960 (1.2e-2) on three frames: clipped, 1500 samples counted.
    let d = run(&[(5, 500), (6, 500), (7, 500)], false);
    assert!(d.detection.flags.clipped);
    assert_eq!(d.detection.clip_count, 1500);
    // 2 of 40 960 (4.9e-5 < 1e-4): not a clipped frame.
    let d = run(&[(12, 2)], false);
    assert!(!d.detection.flags.clipped);
    assert_eq!(d.detection.clip_count, 0);
    // An overloaded provenance sets clipped with no counts (the repository requires it).
    let d = run(&[], true);
    assert!(d.detection.flags.clipped);
    assert_eq!(d.detection.clip_count, 0);
}

#[test]
fn boxes_inside_impulsive_frames_merge_into_one_broadband_detection() {
    let fc = 98e6;
    let mut s = scene_at(fc, 7);
    let mut quiet = flat(BINS);
    add_line(&mut quiet, 1000, 3, 20.0);
    let mut burst = quiet.clone();
    for k in 0..20 {
        add_line(&mut burst, 1200 + 120 * k, 3, 15.0);
    }
    for i in 0..40 {
        let impulsive = (10..16).contains(&i);
        let p = if impulsive { &burst } else { &quiet };
        s.step_with(p, Discontinuity::NONE, ClipCount::NONE, impulsive);
    }
    s.finish();
    let imp: Vec<_> = s
        .out
        .detections
        .iter()
        .filter(|d| d.detection.flags.impulsive)
        .collect();
    assert_eq!(
        imp.len(),
        1,
        "{:#?}",
        s.out
            .detections
            .iter()
            .map(|d| describe(d, FS))
            .collect::<Vec<_>>()
    );
    let d = imp[0];
    assert!(
        d.bins.start <= 1200 && d.bins.end > 1200 + 120 * 19,
        "{}",
        describe(d, FS)
    );
    assert!(d.merged_boxes >= 20, "{}", d.merged_boxes);
    assert!(!d.detection.flags.spur_candidate);
    assert_eq!(d.frames.end - d.frames.start, 6);
    let cw: Vec<_> = s
        .out
        .detections
        .iter()
        .filter(|d| !d.detection.flags.impulsive)
        .collect();
    assert_eq!(cw.len(), 1);
    assert!(cw[0].bins.contains(&1000));
}

#[test]
fn edge_zone_and_low_snr_are_marginal() {
    let fc = 98e6;
    let mut s = scene_at(fc, 8);
    let mut p = flat(BINS);
    add_line(&mut p, 2048 + 1700, 3, 20.0); // beyond the ±8 MHz usable span
    add_line(&mut p, 5, 3, 20.0); // FFT edge bins
    add_line(&mut p, 900, 12, 4.0); // weak: peak SNR < 10 dB
    add_line(&mut p, 2600, 3, 25.0); // strong, central: clean
    for _ in 0..30 {
        s.step(&p);
    }
    s.finish();
    let by_bin = |b: usize| {
        s.out
            .detections
            .iter()
            .filter(move |d| d.bins.contains(&b))
            .collect::<Vec<_>>()
    };
    for b in [2048 + 1700, 5] {
        let v = by_bin(b);
        assert!(!v.is_empty(), "bin {b}");
        assert!(
            v.iter()
                .all(|d| d.detection.flags.edge && d.detection.flags.marginal)
        );
    }
    let weak = by_bin(900);
    assert!(!weak.is_empty());
    assert!(
        weak.iter()
            .all(|d| d.detection.flags.marginal && d.detection.snr_peak_db < 10.0)
    );
    let strong = by_bin(2600);
    assert!(!strong.is_empty());
    assert!(
        strong
            .iter()
            .all(|d| !d.detection.flags.marginal && !d.detection.flags.edge)
    );
}

#[test]
fn quantisation_limited_provenance_marks_every_detection_marginal() {
    let prov = provenance_full(98e6, FS, 8.0, 15e6, false, true);
    let mut s = Scene::new(
        DetectorConfig::new(SurveyId::new()),
        GammaFrames::new(BINS, N_AVG, prov, 9),
    );
    let mut p = flat(BINS);
    add_line(&mut p, 2600, 3, 25.0);
    for _ in 0..10 {
        s.step(&p);
    }
    s.finish();
    assert!(!s.out.detections.is_empty());
    assert!(s.out.detections.iter().all(|d| d.detection.flags.marginal));
    let _ = Rules::default();
}
