//! T-005 noise-floor estimators on synthetic spectra and 8-bit IQ: bias vs occupancy (S4 §3.1),
//! floor-branch Pfa, minimum statistics on carriers, effective averages with overlap,
//! quantisation-limited input, impulsive frames, drift, per-channel floors, gain keying and
//! floor-rise events. Use cases: SPACE-050 (floor survey), AWARE-006 (floor-rise anomaly).

mod common;
mod floor_common;

use common::*;
use floor_common::*;
use hk_core::Discontinuity;
use hk_dsp::floor::{
    FloorConfig, FloorKind, FloorRiseConfig, FloorThreshold, ImpulsiveGateConfig, MinStatConfig,
    MinStatistics, NoiseFloorTracker, effective_averages,
};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};

const SPACE_050: &str = "SPACE-050";
const AWARE_006: &str = "AWARE-006";

#[test]
fn space_050_estimator_bias_vs_occupancy_matches_s4() {
    const FRAMES: usize = 12;
    let mut src = GammaFrames::new(4096, 10, provenance(100e6, 20e6), 1);
    let mut rng = Rng::new(7);
    eprintln!("occupancy | FCME bias dB | p20 bias dB | FCME occupancy | p20 valid frames");
    for occ in [0.0, 0.23, 0.41, 0.61, 0.80] {
        let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
        let (mut fcme, mut p20, mut measured) = (Vec::new(), Vec::new(), Vec::new());
        let mut valid = 0;
        for _ in 0..FRAMES {
            let profile = occupied_profile(&mut rng, 4096, occ);
            let frame = src.next(&profile);
            let f = tracker.update(&frame, |_| {});
            let mut errs: Vec<f64> = f.floor.iter().map(|&v| db32(v)).collect();
            fcme.push(median(&mut errs));
            let p = f.percentile.unwrap();
            p20.push(db32(p.band_floor));
            valid += usize::from(p.valid);
            measured.push(f64::from(f.occupancy));
        }
        let (fe, pe, om) = (median(&mut fcme), median(&mut p20), median(&mut measured));
        eprintln!("{occ:>9.2} | {fe:>+12.3} | {pe:>+11.2} | {om:>14.3} | {valid}/{FRAMES}");
        assert!(
            fe.abs() <= 0.1,
            "{SPACE_050}: FCME bias {fe:.3} dB at {occ} occupancy"
        );
        if occ >= 0.4 {
            assert_eq!(
                valid, 0,
                "{SPACE_050}: p20 must be invalid at {occ} occupancy"
            );
        } else {
            assert_eq!(valid, FRAMES, "{SPACE_050}: p20 valid below 40 % occupancy");
            assert!(pe.abs() < 0.3, "{SPACE_050}: p20 bias {pe:.2} dB at {occ}");
        }
    }
}

#[test]
fn floor_branch_pfa_monte_carlo_within_1_5x_of_design() {
    let mut src = GammaFrames::new(4096, 10, provenance(100e6, 20e6), 2);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let flat = vec![1.0f32; 4096];
    let pfas = [1e-3, 1e-4];
    let thresholds = pfas.map(|p| FloorThreshold::new(10.0, p));
    let mut counts = [0u64; 2];
    let mut cells = 0u64;
    let mut frame = src.empty_frame();
    for _ in 0..300 {
        src.fill(&mut frame, &flat, Discontinuity::NONE);
        let f = tracker.update(&frame, |_| {});
        assert_eq!(f.n_avg_effective, 10.0);
        for (&p, &fl) in frame.spectrum.psd.iter().zip(&f.floor) {
            for (c, t) in counts.iter_mut().zip(&thresholds) {
                *c += u64::from(p > t.level(fl));
            }
        }
        cells += 4096;
    }
    for ((pfa, count), t) in pfas.iter().zip(counts).zip(&thresholds) {
        let ratio = count as f64 / cells as f64 / pfa;
        eprintln!(
            "floor branch T = {:.3} dB, design {pfa:e}: {count} of {cells} cells → {ratio:.2}× design",
            t.multiplier_db()
        );
        assert!(
            (1.0 / 1.5..=1.5).contains(&ratio),
            "Pfa {pfa}: {ratio:.2}× design"
        );
    }
}

#[test]
fn min_statistics_reads_continuous_carriers_as_floor_but_not_intermittent_channels() {
    let bins = 512;
    let mut src = GammaFrames::new(bins, 10, provenance(144e6, 1e6), 3);
    let cfg = MinStatConfig {
        window_frames: 128,
        ..MinStatConfig::default()
    };
    let mut minstat = MinStatistics::new(cfg, bins, 10.0).unwrap();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut profile = vec![1.0f32; bins];
    profile[100..130].fill(101.0); // continuous carrier, 20 dB
    profile[300..310].fill(11.0); // continuous carrier, 10 dB
    let (mut ms_carrier, mut fc_carrier, mut ms_idle, mut ms_noise) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for k in 0..400 {
        let on = (k / 40) % 2 == 0;
        profile[400..440].fill(if on { 31.0 } else { 1.0 }); // intermittent, 15 dB
        let frame = src.next(&profile);
        let f = tracker.update(&frame, |_| {});
        minstat.update(&frame.spectrum.psd);
        if k >= 300 {
            let ms = minstat.floor();
            ms_carrier.extend(ms[100..130].iter().chain(&ms[300..310]).map(|&v| db32(v)));
            fc_carrier.extend(
                f.floor[100..130]
                    .iter()
                    .chain(&f.floor[300..310])
                    .map(|&v| db32(v)),
            );
            ms_idle.extend(ms[400..440].iter().map(|&v| db32(v)));
            ms_noise.extend(ms[150..280].iter().map(|&v| db32(v)));
        }
    }
    assert!(minstat.is_ready());
    let (msc, fcc) = (median(&mut ms_carrier), median(&mut fc_carrier));
    let (msi, msn) = (median(&mut ms_idle), median(&mut ms_noise));
    eprintln!(
        "carrier bins: min-stat {msc:+.2} dB, FCME {fcc:+.3} dB; intermittent min-stat {msi:+.2} dB; noise min-stat {msn:+.2} dB"
    );
    assert!(
        msc > 4.0,
        "min-stat must show its carrier bias (S4: +4 to +14 dB), got {msc:.2}"
    );
    assert!(fcc.abs() < 0.3, "FCME on carrier bins {fcc:.3} dB");
    assert!(
        msi.abs() < 0.5,
        "min-stat on an intermittent channel {msi:.2} dB"
    );
    assert!(msn.abs() < 0.3, "min-stat on noise {msn:.2} dB");
}

#[test]
fn effective_averages_match_measured_variance_and_pfa_with_overlap() {
    let fs = 1e6;
    let prov = provenance(433.92e6, fs);
    let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(256), 10)).unwrap();
    let mut rng = Rng::new(4);
    let iq = synth::complex_noise(&mut rng, 1_280_128, 1e-3);
    let h = header(0, &prov, Discontinuity::STREAM_START);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let (mut s1, mut s2) = (vec![0.0f64; 256], vec![0.0f64; 256]);
    let mut frames = 0usize;
    let (mut exceed, mut cells) = (0u64, 0u64);
    let mut n_eff = 0.0;
    stft.push(InputInfo::from(&h), &iq, |frame| {
        n_eff = effective_averages(&frame.spectrum.resolution);
        let thr = FloorThreshold::new(n_eff, 1e-3);
        let f = tracker.update(frame, |_| {});
        for (i, (&p, &fl)) in frame.spectrum.psd.iter().zip(&f.floor).enumerate() {
            s1[i] += f64::from(p);
            s2[i] += f64::from(p) * f64::from(p);
            exceed += u64::from(p > thr.level(fl));
        }
        cells += 256;
        frames += 1;
    });
    assert!(frames >= 990);
    let n = frames as f64;
    let measured: f64 = s1
        .iter()
        .zip(&s2)
        .map(|(&a, &b)| {
            let m = a / n;
            m * m / (b / n - m * m)
        })
        .sum::<f64>()
        / 256.0;
    let ratio = exceed as f64 / cells as f64 / 1e-3;
    eprintln!(
        "Hann 50% overlap, K = 10: n_eff model {n_eff:.3}, measured mean²/var {measured:.3}; Pfa 1e-3 → {ratio:.2}× design"
    );
    assert!((measured / n_eff - 1.0).abs() < 0.03);
    assert!((1.0 / 1.5..=1.5).contains(&ratio));
}

fn quantised_frames(sigma_codes: f64, dc_codes: (f64, f64), seed: u64) -> Vec<(bool, f64)> {
    let fs = 20e6;
    let prov = provenance_with(98e6, fs, 8.0);
    let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(4096), 10)).unwrap();
    let mut rng = Rng::new(seed);
    let len = 8 * 10 * 2048 + 2048;
    let variance = 2.0 * (sigma_codes / 128.0).powi(2);
    let mut analog = synth::complex_noise(&mut rng, len, variance);
    for s in &mut analog {
        s.re += (dc_codes.0 / 128.0) as f32;
        s.im += (dc_codes.1 / 128.0) as f32;
    }
    let (ci8, clipped) = synth::quantize_ci8(&analog);
    assert_eq!(clipped, 0);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut out = Vec::new();
    let h = header(0, &prov, Discontinuity::STREAM_START);
    stft.push(InputInfo::from(&h), &ci8, |frame| {
        let f = tracker.update(frame, |_| {});
        out.push((
            f.quantisation_limited,
            f64::from(f.band_floor_dbfs_per_hz(FloorKind::Frame)),
        ));
    });
    assert_eq!(out.len(), 8);
    out
}

#[test]
fn space_050_quantisation_limited_flag_on_low_gain_8bit_input() {
    // S4 low gain: rail std ≈ 0.35–0.5 codes with a (+0.6, −2.3)-code DC offset.
    for (sigma, dc, want) in [
        (0.35, (0.6, -2.3), true),
        (0.35, (0.0, 0.0), true),
        (3.0, (0.6, -2.3), false),
        (12.0, (0.0, 0.0), false),
    ] {
        let frames = quantised_frames(sigma, dc, 0x9a17 + sigma as u64);
        let floor = frames.iter().map(|f| f.1).sum::<f64>() / frames.len() as f64;
        eprintln!(
            "σ = {sigma} codes, DC {dc:?}: floor {floor:.1} dBFS/Hz, limited {}",
            frames[0].0
        );
        assert!(
            frames.iter().all(|f| f.0 == want),
            "{SPACE_050}: quantisation_limited should be {want} at σ = {sigma} codes (floor {floor:.1})"
        );
    }
}

#[test]
fn impulsive_frames_are_gated_and_do_not_move_the_slow_floor() {
    let run = |gate: bool| {
        let bins = 1024;
        let mut src = GammaFrames::new(bins, 10, provenance(98e6, 1e6), 5);
        let mut config = FloorConfig::default();
        if !gate {
            config.impulsive = ImpulsiveGateConfig {
                rise_db: 1e9,
                ..ImpulsiveGateConfig::default()
            };
            config.rise = FloorRiseConfig {
                threshold_db: 1e9,
                ..FloorRiseConfig::default()
            };
        }
        let mut tracker = NoiseFloorTracker::new(config).unwrap();
        let flat = vec![1.0f32; bins];
        let hot = vec![10.0f32; bins]; // one 2-ms-style broadband burst: +10 dB
        let (mut hits, mut false_flags, mut injected, mut slow) = (0, 0, 0, Vec::new());
        let mut frame_floor_on_hot = Vec::new();
        for k in 0..800 {
            let impulse = k >= 50 && k % 25 == 0;
            let frame = src.next(if impulse { &hot } else { &flat });
            let f = tracker.update(&frame, |_| panic!("an impulsive frame is not a floor rise"));
            if impulse {
                injected += 1;
                hits += usize::from(f.impulsive);
                frame_floor_on_hot.push(f64::from(f.band_floor_dbfs_per_hz(FloorKind::Frame)));
            } else {
                false_flags += usize::from(f.impulsive);
            }
            if k >= 400 {
                slow.push(f64::from(f.band_floor_dbfs_per_hz(FloorKind::Slow)));
            }
        }
        (
            hits,
            injected,
            false_flags,
            median(&mut slow),
            median(&mut frame_floor_on_hot),
        )
    };
    let (hits, injected, false_flags, slow, hot) = run(true);
    let (_, _, _, slow_ungated, _) = run(false);
    eprintln!(
        "gate: {hits}/{injected} impulsive frames flagged, {false_flags} false flags; slow floor {slow:+.3} dB (ungated {slow_ungated:+.3} dB); per-frame floor on bursts {hot:+.2} dB"
    );
    assert_eq!(hits, injected);
    assert!(
        false_flags <= 7,
        "{false_flags} false impulsive flags in ~770 frames"
    );
    assert!(slow.abs() < 0.1, "gated slow floor {slow:.3} dB");
    assert!(
        slow_ungated > 0.5,
        "without the gate the slow floor is biased ({slow_ungated:.3})"
    );
    assert!(
        (hot - 10.0).abs() < 0.2,
        "per-frame floor absorbs the burst ({hot:.2})"
    );
}

#[test]
fn space_050_drifting_floor_tracked_within_1_db() {
    let bins = 1024;
    let mut src = GammaFrames::new(bins, 10, provenance(30e6, 1e6), 6);
    let period = src.frame_period_s();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let frames = 1000;
    let drift_db = 4.0;
    let mut profile = vec![1.0f32; bins];
    let (mut worst_frame, mut worst_slow) = (0.0f64, 0.0f64);
    for k in 0..frames {
        let truth = drift_db * k as f64 / frames as f64;
        let level = 10f64.powf(truth / 10.0) as f32;
        profile.fill(level);
        if k % 10 < 3 {
            profile[600..650].fill(level * 30.0); // intermittent burst, 30 % duty
        }
        let frame = src.next(&profile);
        let f = tracker.update(&frame, |e| {
            panic!("{AWARE_006}: drift is not a rise: {e:?}")
        });
        let fe = f64::from(f.band_floor_dbfs_per_hz(FloorKind::Frame)) - truth;
        worst_frame = worst_frame.max(fe.abs());
        if f.slow_ready {
            let se = f64::from(f.band_floor_dbfs_per_hz(FloorKind::Slow)) - truth;
            worst_slow = worst_slow.max(se.abs());
        }
    }
    eprintln!(
        "drift {drift_db} dB over {:.1} s: worst per-frame error {worst_frame:.3} dB, worst slow-floor error {worst_slow:.3} dB (IIR τ 1 s)",
        frames as f64 * period
    );
    assert!(worst_frame < 0.3);
    assert!(
        worst_slow < 1.0,
        "{SPACE_050}: slow floor lags drift by {worst_slow:.2} dB"
    );
    assert_eq!(tracker.stats().rise_events, 0);
}

#[test]
fn aware_006_partial_band_floor_rise_event_time_extent_and_step() {
    let bins = 4096;
    let mut src = GammaFrames::new(bins, 10, provenance(1575.42e6, 2e6), 8);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut events = Vec::new();
    let mut profile = vec![1.0f32; bins];
    let mut after = (Vec::new(), Vec::new());
    for k in 0..200 {
        if k == 120 {
            profile[1000..2000].fill(10.0);
        }
        let frame = src.next(&profile);
        let f = tracker.update(&frame, |e| events.push(e.clone()));
        assert!(
            !f.impulsive || k < 120,
            "a partial-band rise does not flag frames impulsive"
        );
        if k >= 150 {
            after
                .0
                .extend(f.slow_floor[1200..1800].iter().map(|&v| db32(v)));
            after
                .1
                .extend(f.slow_floor[2600..3800].iter().map(|&v| db32(v)));
        }
    }
    assert_eq!(events.len(), 1, "{AWARE_006}: one floor-rise event");
    let e = &events[0];
    eprintln!(
        "{AWARE_006}: event seq {}..{} bins {:?} step {:+.2} dB ({:.1} → {:.1} dBFS/Hz)",
        e.start_seq,
        e.confirmed_seq,
        e.bins,
        e.step_db,
        e.floor_before_dbfs_per_hz,
        e.floor_after_dbfs_per_hz
    );
    assert_eq!(e.start_seq, 120);
    assert_eq!(e.confirmed_seq, 124);
    assert!((e.step_db - 10.0).abs() < 0.5);
    assert!(e.bins.start.abs_diff(1000) <= 128 && e.bins.end.abs_diff(2000) <= 128);
    let bw = 2e6 / 4096.0;
    assert!((e.f_lo_hz - (1575.42e6 + (e.bins.start as f64 - 2048.0) * bw - bw / 2.0)).abs() < 1.0);
    let (inside, outside) = (median(&mut after.0), median(&mut after.1));
    assert!(
        (inside - 10.0).abs() < 1.0 && outside.abs() < 0.2,
        "{inside:.2} / {outside:.2}"
    );
}

#[test]
fn per_channel_floors_follow_a_shaped_floor() {
    let bins = 1024;
    let mut src = GammaFrames::new(bins, 10, provenance(446e6, 1e6), 9);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut profile = vec![1.0f32; bins];
    profile[512..].fill(4.0);
    profile[760..770].fill(400.0); // a carrier inside channel 2
    let channels = [100..300, 700..900, 0..1024];
    let mut out = [hk_dsp::ChannelFloor {
        start_bin: 0,
        end_bin: 0,
        floor: 0.0,
        dbfs_per_hz: 0.0,
        dbfs: 0.0,
        uncertainty_db: 0.0,
        quantisation_limited: false,
    }; 3];
    for _ in 0..40 {
        let frame = src.next(&profile);
        tracker.update(&frame, |_| {});
    }
    let f = tracker.last().unwrap();
    for kind in [FloorKind::Frame, FloorKind::Slow] {
        f.channel_floors(&channels, kind, &mut out);
        eprintln!(
            "{kind:?}: {:.2} / {:.2} dB",
            out[0].dbfs_per_hz, out[1].dbfs_per_hz
        );
        assert!(f64::from(out[0].dbfs_per_hz).abs() < 0.2);
        assert!((f64::from(out[1].dbfs_per_hz) - 6.02).abs() < 0.2);
        assert!(
            (out[0].dbfs - (out[0].dbfs_per_hz + 10.0 * (200.0f32 * 1e6 / 1024.0).log10())).abs()
                < 1e-3
        );
        assert_eq!(out[0].uncertainty_db, 0.5);
    }
}

#[test]
fn gain_change_and_gap_start_new_segments_without_rise_events() {
    let bins = 1024;
    let low = provenance_with(915e6, 2e6, 16.0);
    let high = provenance_with(915e6, 2e6, 24.0);
    let mut src = GammaFrames::new(bins, 10, low, 10);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut frame = src.empty_frame();
    let one = vec![1.0f32; bins];
    let ten = vec![10.0f32; bins];
    for k in 0..60 {
        if k == 30 {
            src.provenance = high.clone();
        }
        let flags = if k == 45 {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        src.fill(&mut frame, if k < 30 { &one } else { &ten }, flags);
        let f = tracker.update(&frame, |e| panic!("gain step is not a rise: {e:?}"));
        match k {
            0 => assert!(f.reset && f.segment == 0),
            30 => {
                assert!(f.reset && f.segment == 1 && f.gain.lna_db == 24.0);
                assert!((f64::from(f.band_floor_dbfs_per_hz(FloorKind::Slow)) - 10.0).abs() < 0.2);
            }
            45 => assert!(f.reset && f.segment == 2),
            _ => assert!(!f.reset),
        }
    }
    assert_eq!(tracker.stats().resets, 3);
}
