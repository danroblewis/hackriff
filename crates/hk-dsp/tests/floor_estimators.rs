//! T-005 noise-floor estimators on synthetic spectra and 8-bit IQ: bias vs occupancy (S4 §3.1),
//! floor-branch Pfa, the wide-signal reference, minimum statistics on carriers, effective
//! averages with overlap, quantisation-limited input, the impulsive gate and its release on
//! sustained steps, drift, per-channel floors, gain keying, FCME input guards and floor-change
//! episodes. Use cases: SPACE-050 (floor survey), AWARE-006 (floor-rise anomaly).

mod common;
mod floor_common;

use std::ops::Range;

use common::*;
use floor_common::*;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{
    EndReason, FloorChangeClass, FloorChangeConfig, FloorConfig, FloorEvent, FloorEventKind,
    FloorKind, FloorThreshold, ImpulsiveGateConfig, MinStatConfig, MinStatistics,
    NoiseFloorTracker, effective_averages,
};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_model::Provenance;

const SPACE_050: &str = "SPACE-050";
const AWARE_006: &str = "AWARE-006";

fn count(events: &[FloorEvent], kind: FloorEventKind) -> usize {
    events.iter().filter(|e| e.kind == kind).count()
}

#[test]
fn space_050_estimator_bias_vs_occupancy_matches_s4() {
    const FRAMES: usize = 12;
    let mut src = GammaFrames::new(4096, 10, provenance(100e6, 20e6), 1);
    let mut rng = Rng::new(7);
    eprintln!("occupancy | FCME bias dB | p20 bias dB | occupancy | p20 valid frames");
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
        eprintln!("{occ:>9.2} | {fe:>+12.3} | {pe:>+11.2} | {om:>9.3} | {valid}/{FRAMES}");
        assert!(
            fe.abs() <= 0.1,
            "{SPACE_050}: FCME bias {fe:.3} dB at {occ} occupancy"
        );
        // p20 is valid below 40 % occupancy (S4). At 41 % the occupancy estimate sits on that
        // boundary frame to frame (0.41 ± noise), so p20 must be invalid in at least half the
        // frames there, and in all of them further above.
        if (0.40..0.45).contains(&occ) {
            assert!(
                valid <= FRAMES / 2,
                "{SPACE_050}: p20 mostly invalid at {occ} occupancy ({valid}/{FRAMES} valid)"
            );
        } else if occ >= 0.45 {
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
fn floor_branch_pfa_monte_carlo_within_1_5x_of_design_for_frame_and_wide_references() {
    let mut src = GammaFrames::new(4096, 10, provenance(100e6, 20e6), 2);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let flat = vec![1.0f32; 4096];
    let pfas = [1e-3, 1e-4];
    let thresholds = pfas.map(|p| FloorThreshold::single(10.0, p));
    let mut counts = [[0u64; 2]; 2];
    let mut cells = 0u64;
    let mut frame = src.empty_frame();
    for _ in 0..300 {
        src.fill(&mut frame, &flat, Discontinuity::NONE);
        let f = tracker.update(&frame, |_| {});
        assert_eq!(f.n_avg_effective, 10.0);
        for (r, kind) in [FloorKind::Frame, FloorKind::Wide].into_iter().enumerate() {
            for (&p, &fl) in frame.spectrum.psd.iter().zip(f.trace(kind)) {
                for (c, t) in counts[r].iter_mut().zip(&thresholds) {
                    *c += u64::from(p > t.level_on(fl));
                }
            }
        }
        cells += 4096;
    }
    for (r, name) in ["frame", "wide"].into_iter().enumerate() {
        for ((pfa, &count), t) in pfas.iter().zip(&counts[r]).zip(&thresholds) {
            let ratio = count as f64 / cells as f64 / pfa;
            eprintln!(
                "{name} reference, T = {:.3} dB, design {pfa:e}: {count} of {cells} cells → {ratio:.2}× design",
                t.on_db()
            );
            assert!(
                (1.0 / 1.5..=1.5).contains(&ratio),
                "{name} Pfa {pfa}: {ratio:.2}×"
            );
        }
    }
}

#[test]
fn wide_reference_keeps_the_interior_of_a_2048_bin_signal() {
    let mut src = GammaFrames::new(4096, 10, provenance(100e6, 20e6), 12);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut profile = vec![1.0f32; 4096];
    profile[1024..3072].fill(11.0); // +10 dB over the floor
    let t = FloorThreshold::s4(10.0);
    let (mut wide_on, mut wide_guard, mut frame_on, mut frame_guard, mut total) = (0, 0, 0, 0, 0);
    let (mut inside_frame, mut inside_wide) = (Vec::new(), Vec::new());
    for _ in 0..20 {
        let frame = src.next(&profile);
        let f = tracker.update(&frame, |_| {});
        for i in 1024..3072 {
            let p = frame.spectrum.psd[i];
            wide_on += usize::from(p > t.level_on(f.wide_floor[i]));
            wide_guard += usize::from(p > t.guard_level(f.wide_floor[i]));
            frame_on += usize::from(p > t.level_on(f.floor[i]));
            frame_guard += usize::from(p > t.guard_level(f.floor[i]));
            total += 1;
        }
        inside_frame.push(db32(f.floor[2048]));
        inside_wide.push(db32(f.wide_floor[2048]));
    }
    let pct = |n: usize| 100.0 * n as f64 / total as f64;
    let (fi, wi) = (median(&mut inside_frame), median(&mut inside_wide));
    eprintln!(
        "2048-bin +10 dB signal: frame floor inside {fi:+.2} dB (on {:.1} %, guard {:.1} %); wide reference {wi:+.2} dB (on {:.1} %, guard {:.1} %)",
        pct(frame_on),
        pct(frame_guard),
        pct(wide_on),
        pct(wide_guard)
    );
    assert!(fi > 9.0, "the per-frame floor reads the signal ({fi:.2})");
    assert!(
        wi.abs() < 0.3,
        "the wide reference stays at the floor ({wi:.2})"
    );
    assert!(pct(wide_on) >= 95.0 && pct(wide_guard) >= 95.0);
    assert!(
        pct(frame_on) < 50.0,
        "documented failure of the per-frame floor"
    );
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
    profile[100..130].fill(101.0);
    profile[300..310].fill(11.0);
    let (mut ms_carrier, mut fc_carrier, mut ms_idle, mut ms_noise) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for k in 0..400 {
        let on = (k / 40) % 2 == 0;
        profile[400..440].fill(if on { 31.0 } else { 1.0 });
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
        "min-stat must show its carrier bias, got {msc:.2}"
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
        let thr = FloorThreshold::single(n_eff, 1e-3);
        let f = tracker.update(frame, |_| {});
        for (i, (&p, &fl)) in frame.spectrum.psd.iter().zip(&f.wide_floor).enumerate() {
            s1[i] += f64::from(p);
            s2[i] += f64::from(p) * f64::from(p);
            exceed += u64::from(p > thr.level_on(fl));
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
            config.change.threshold_db = 1e9;
            config.change.end_threshold_db = 1e9;
            config.slow.settle_db = 1e9;
        }
        let mut tracker = NoiseFloorTracker::new(config).unwrap();
        let flat = vec![1.0f32; bins];
        let hot = vec![10.0f32; bins];
        let (mut hits, mut false_flags, mut injected, mut slow) = (0, 0, 0, Vec::new());
        let mut frame_floor_on_hot = Vec::new();
        for k in 0..800 {
            let impulse = k >= 50 && k % 25 == 0;
            let frame = src.next(if impulse { &hot } else { &flat });
            let f = tracker.update(&frame, |e| {
                panic!("an impulsive frame is not a floor change: {e:?}")
            });
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
        assert_eq!(tracker.stats().gate_releases, 0);
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
    assert!(false_flags <= 7);
    assert!(slow.abs() < 0.1, "gated slow floor {slow:.3} dB");
    assert!(slow_ungated > 0.5);
    assert!((hot - 10.0).abs() < 0.2);
}

struct StepOutcome {
    events: Vec<FloorEvent>,
    late_flags: usize,
    inside_err_db: f64,
    outside_err_db: f64,
    releases: u64,
}

/// 100 base frames, then `region` raised by `step_db` for 600 frames (6.1 s); optional start-up
/// transient frames first.
fn sustained_step(step_db: f64, region: Range<usize>, transient: &[f32], seed: u64) -> StepOutcome {
    let bins = 1024;
    let mut src = GammaFrames::new(bins, 10, provenance(98e6, 1e6), seed);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut events = Vec::new();
    for &level in transient {
        let frame = src.next(&vec![level; bins]);
        tracker.update(&frame, |e| events.push(e.clone()));
    }
    let mut profile = vec![1.0f32; bins];
    let mut late_flags = 0;
    let total = 700;
    for k in 0..total {
        if k == 100 {
            profile[region.clone()].fill(10f32.powf(step_db as f32 / 10.0));
        }
        let frame = src.next(&profile);
        let f = tracker.update(&frame, |e| events.push(e.clone()));
        if k >= total - 200 {
            late_flags += usize::from(f.impulsive);
        }
    }
    let f = tracker.last().unwrap();
    let slow_db = |r: Range<usize>| {
        let mut v: Vec<f64> = f.slow_floor[r].iter().map(|&x| db32(x)).collect();
        median(&mut v)
    };
    let inner = region.start + 160..region.end - 160;
    let outside = if region.end < bins {
        region.end + 160..bins
    } else {
        0..0
    };
    StepOutcome {
        events,
        late_flags,
        inside_err_db: slow_db(inner) - if step_db > 0.0 { step_db } else { 0.0 },
        outside_err_db: if outside.is_empty() {
            0.0
        } else {
            slow_db(outside)
        },
        releases: tracker.stats().gate_releases,
    }
}

#[test]
fn gate_releases_on_sustained_sub_threshold_steps_and_rises_only_at_threshold() {
    for (step, region, want_rises) in [
        (1.0, 0..1024, 0),
        (2.0, 0..1024, 0),
        (2.9, 0..1024, 0),
        (1.5, 0..614, 0), // 60 % of the band
        (4.0, 0..1024, 1),
    ] {
        let o = sustained_step(step, region.clone(), &[], 20 + (step * 10.0) as u64);
        eprintln!(
            "{step} dB over bins {region:?}: gate releases {}, late impulsive flags {}, slow floor error inside {:+.3} dB / outside {:+.3} dB, rises {}, falls {}",
            o.releases,
            o.late_flags,
            o.inside_err_db,
            o.outside_err_db,
            count(&o.events, FloorEventKind::Rise),
            count(&o.events, FloorEventKind::Fall)
        );
        assert!(
            o.releases >= 1,
            "the gate released on a sustained {step} dB step"
        );
        assert_eq!(
            o.late_flags, 0,
            "gate still locked on after a {step} dB step"
        );
        // A confirmed rise without SK is Unverified: the slow floor stays at the baseline (T-029).
        let want_err = if want_rises > 0 { -step } else { 0.0 };
        assert!(
            (o.inside_err_db - want_err).abs() < 0.15,
            "slow floor error {:.3} dB, want {want_err:.1} dB",
            o.inside_err_db
        );
        assert!(o.outside_err_db.abs() < 0.15);
        assert_eq!(
            count(&o.events, FloorEventKind::Rise),
            want_rises,
            "{AWARE_006}: {step} dB"
        );
        assert_eq!(count(&o.events, FloorEventKind::Fall), 0);
    }
}

#[test]
fn startup_transient_does_not_lock_the_gate() {
    // +10 dB on the first frame, +1.8 dB for four more, then the true floor.
    let o = sustained_step(0.0, 0..1024, &[10.0, 1.51, 1.51, 1.51, 1.51], 31);
    eprintln!(
        "start-up transient: late flags {}, slow floor error {:+.3} dB, events {}",
        o.late_flags,
        o.inside_err_db,
        o.events.len()
    );
    assert_eq!(o.late_flags, 0);
    assert!(o.inside_err_db.abs() < 0.15);
    assert!(o.events.is_empty());
}

#[test]
fn aware_006_bursty_wide_signal_makes_no_floor_rise_episodes() {
    // 20 Msps-like frame period (2.05 ms), a 34 %-of-band +10 dB signal.
    let bins = 2048;
    for (on, off, frames) in [(10, 10, 900), (244, 244, 1500)] {
        let mut src = GammaFrames::new(bins, 10, provenance(2437e6, 10e6), 40 + on as u64);
        let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
        let mut events = Vec::new();
        let mut profile = vec![1.0f32; bins];
        let mut max_active = 0;
        for k in 0..frames {
            let lit = k % (on + off) < on;
            profile[700..1400].fill(if lit { 11.0 } else { 1.0 });
            let frame = src.next(&profile);
            let f = tracker.update(&frame, |e| events.push(e.clone()));
            max_active = max_active.max(f.active_episodes);
        }
        let f = tracker.last().unwrap();
        let mut inside: Vec<f64> = f.slow_floor[900..1200].iter().map(|&x| db32(x)).collect();
        let inside = median(&mut inside);
        eprintln!(
            "bursty {on}/{off} frames ({:.2}/{:.2} s): {} events ({} interrupted falls), max active episodes {max_active}, slow floor under the signal {inside:+.2} dB",
            on as f64 * src.frame_period_s(),
            off as f64 * src.frame_period_s(),
            events.len(),
            events
                .iter()
                .filter(|e| e.kind == FloorEventKind::Fall && e.interrupted)
                .count()
        );
        // The signal was on during warm-up, so the slow floor starts on it; the correction is
        // reported as an interrupted Fall. Nothing else.
        assert!(
            events
                .iter()
                .all(|e| e.kind == FloorEventKind::Fall && e.interrupted),
            "{AWARE_006}: {events:?}"
        );
        assert_eq!(max_active, 0);
        assert!(
            inside.abs() < 0.5,
            "slow floor pulled up by the bursty signal"
        );
    }
}

/// Noise at 1e-3 plus, from `t0`, extra noise of `extra` variance: steady, or gated to the first
/// 30 % of every 5120-sample frame period.
fn floor_change_iq(extra: f64, gated: bool, seed: u64) -> Vec<num_complex::Complex32> {
    let n = 5120 * 293; // 1.5 s at 1 Msps
    let t0 = 300_160;
    let mut rng = Rng::new(seed);
    let mut iq = synth::complex_noise(&mut rng, n, 1e-3);
    let rise = synth::complex_noise(&mut rng, n - t0, extra);
    for (i, (x, r)) in iq[t0..].iter_mut().zip(&rise).enumerate() {
        if !gated || (t0 + i) % 5120 < 1536 {
            *x += *r;
        }
    }
    iq
}

#[test]
fn aware_006_spectral_kurtosis_separates_noise_rises_from_structured_signals() {
    let fs = 1e6;
    let prov = provenance(1575.42e6, fs);
    let cfg = FloorConfig {
        change: FloorChangeConfig {
            confirm_s: 0.5,
            end_s: 0.5,
            ..FloorChangeConfig::default()
        },
        ..FloorConfig::default()
    };
    let mut quiet = cfg;
    quiet.change.emit_structured = false;
    let h = header(0, &prov, Discontinuity::STREAM_START);
    let run = |iq: &[num_complex::Complex32], cfg: FloorConfig| {
        let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(1024), 10)).unwrap();
        let mut tracker = NoiseFloorTracker::new(cfg).unwrap();
        let mut events = Vec::new();
        stft.push(InputInfo::from(&h), iq, |frame| {
            tracker.update(frame, |e| events.push(e.clone()));
        });
        (events, tracker.stats())
    };

    // Steady +6 dB noise rise at 0.30016 s.
    let steady = floor_change_iq(3e-3, false, 50);
    let (events, _) = run(&steady, cfg);
    assert_eq!(events.len(), 1, "{events:?}");
    let e = &events[0];
    let onset = e.onset_t.sample_index as f64 / fs;
    let confirm = e.confirmed_t.sample_index as f64 / fs;
    eprintln!(
        "{AWARE_006} steady rise: class {:?}, SK {:?}, excess std {:.2} dB, step {:+.2} ± {:.3} dB, onset {onset:.4} s, confirmed {confirm:.4} s",
        e.class, e.sk, e.excess_std_db, e.step_db, e.step_uncertainty_db
    );
    assert_eq!(
        (e.kind, e.class),
        (FloorEventKind::Rise, FloorChangeClass::NoiseLike)
    );
    assert!((e.sk.unwrap() - 1.0).abs() < 0.15);
    assert!((f64::from(e.step_db) - 6.02).abs() < 0.3);
    assert!((onset - 0.30016).abs() <= 5120.0 / fs);
    assert!((confirm - onset - 0.5).abs() <= 5120.0 / fs);

    // Same average power, but bursty within each frame (SK ≫ 1): structured, emitted with that
    // class by default and suppressed with `emit_structured = false`.
    let bursty = floor_change_iq(20e-3, true, 51);
    let (events, stats) = run(&bursty, quiet);
    assert!(
        events.is_empty(),
        "structured signal reported with emit_structured off: {events:?}"
    );
    assert_eq!(stats.structured_episodes, 1);
    let (events, _) = run(&bursty, cfg);
    assert_eq!(events.len(), 1, "{events:?}");
    let e = &events[0];
    eprintln!(
        "{AWARE_006} bursty wide signal: class {:?}, SK {:?}, step {:+.2} dB",
        e.class, e.sk, e.step_db
    );
    assert_eq!(
        (e.kind, e.class),
        (FloorEventKind::Rise, FloorChangeClass::Structured)
    );
    assert!(e.sk.unwrap() > 1.5);
}

#[test]
fn aware_006_episode_rise_end_and_holdoff() {
    let bins = 1024;
    let mut src = GammaFrames::new(bins, 10, provenance(1575.42e6, 1e6), 60);
    let period = src.frame_period_s();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut events = Vec::new();
    let mut profile = vec![1.0f32; bins];
    let (mut during, mut after) = (0.0, 0.0);
    for k in 0..1000u64 {
        let lit = (100..400).contains(&k) || k >= 520;
        profile[300..700].fill(if lit { 4.0 } else { 1.0 });
        let frame = src.next(&profile);
        let f = tracker.update(&frame, |e| events.push(e.clone()));
        if k == 390 {
            during = db32(f.slow_floor[500]);
        }
        if k == 515 {
            after = db32(f.slow_floor[500]);
        }
    }
    for e in &events {
        eprintln!(
            "{:?} episode {} {:?}: onset seq {} confirmed {} duration {:.2} s bins {:?} step {:+.2} dB peak {:+.2} baseline {:.2} dBFS/Hz seg {}",
            e.kind,
            e.episode,
            e.class,
            e.onset_seq,
            e.confirmed_seq,
            e.duration_s,
            e.bins,
            e.step_db,
            e.peak_step_db,
            e.baseline_dbfs_per_hz,
            e.baseline_segment
        );
    }
    let confirm = (1.0 / period).ceil() as u64;
    let holdoff = (2.0 / period).ceil() as u64;
    assert_eq!(events.len(), 3, "rise, end, rise after hold-off");
    let (r1, end, r2) = (&events[0], &events[1], &events[2]);
    assert_eq!(
        (r1.kind, r1.class),
        (FloorEventKind::Rise, FloorChangeClass::Unverified)
    );
    assert_eq!((r1.onset_seq, r1.confirmed_seq), (100, 100 + confirm - 1));
    assert!((r1.step_db - 6.02).abs() < 0.3 && r1.step_uncertainty_db < 0.1);
    assert!(r1.bins.start.abs_diff(300) <= 128 && r1.bins.end.abs_diff(700) <= 128);
    assert_eq!(
        (end.kind, end.end_reason, end.episode),
        (FloorEventKind::End, Some(EndReason::Returned), r1.episode)
    );
    assert_eq!((end.onset_seq, end.confirmed_seq), (400, 400 + confirm - 1));
    assert!((end.duration_s - 300.0 * period).abs() < 1e-6);
    assert!(end.peak_step_db >= r1.step_db);
    assert_eq!(r2.kind, FloorEventKind::Rise);
    assert_ne!(r2.episode, r1.episode);
    assert_eq!(
        (r2.onset_seq, r2.confirmed_seq),
        (520, end.confirmed_seq + holdoff),
        "a rise inside the hold-off keeps its onset and confirms when the hold-off expires"
    );
    // Unverified (no SK): the slow floor stays at the baseline during the episode (T-029).
    assert!(
        during.abs() < 0.3 && after.abs() < 0.3,
        "{during} / {after}"
    );
}

fn provenance_full(
    center_hz: f64,
    bandwidth_hz: f64,
    lna_db: f64,
    port: Option<&str>,
) -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:hk-dsp-test",
        "tune": {"center_hz": center_hz, "sample_rate_hz": 2e6, "lna_db": lna_db, "vga_db": 20.0,
                 "amp_on": false, "bandwidth_hz": bandwidth_hz},
        "overload": false, "quantisation_limited": false, "antenna_port": port,
        "clock_source": "internal", "clock_locked": true, "timestamp_method": "synthetic",
        "timestamp_error_budget_ns": 0,
    });
    ProvenanceHandle::new(serde_json::from_value::<Provenance>(json).expect("provenance JSON"))
}

#[test]
fn gain_key_tolerance_resets_and_reset_closes_episodes() {
    let bins = 1024;
    let base = provenance_full(915e6, 1.75e6, 16.0, Some("A"));
    let mut src = GammaFrames::new(bins, 10, base.clone(), 10);
    let cfg = FloorConfig {
        change: FloorChangeConfig {
            confirm_s: 0.05, // 10 frames at 5.12 ms
            ..FloorChangeConfig::default()
        },
        ..FloorConfig::default()
    };
    let mut tracker = NoiseFloorTracker::new(cfg).unwrap();
    let mut events = Vec::new();
    let mut frame = src.empty_frame();
    let one = vec![1.0f32; bins];
    let ten = vec![10.0f32; bins];
    for k in 0..90 {
        src.provenance = match k {
            0..10 => base.clone(),
            10..20 => provenance_full(915e6 + 0.3, 1.75e6, 16.0, Some("A")), // sub-Hz correction
            20..30 => provenance_full(915e6 + 0.3, 2.5e6, 16.0, Some("A")),  // bandwidth
            30..60 => provenance_full(915e6 + 0.3, 2.5e6, 16.0, Some("B")),  // antenna port
            _ => provenance_full(915e6 + 0.3, 2.5e6, 24.0, Some("B")),       // gain
        };
        let flags = if k == 80 {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        let level = if (40..60).contains(&k) || k >= 60 {
            &ten
        } else {
            &one
        };
        src.fill(&mut frame, level, flags);
        let f = tracker.update(&frame, |e| events.push(e.clone()));
        let want_reset = matches!(k, 0 | 20 | 30 | 60 | 80);
        assert_eq!(f.reset, want_reset, "frame {k}");
    }
    assert_eq!(tracker.stats().resets, 5);
    let kinds: Vec<_> = events.iter().map(|e| (e.kind, e.end_reason)).collect();
    eprintln!("events across resets: {kinds:?}");
    // A gain change makes the floor incomparable: the episode closes as Unknown at the reset;
    // the next segment warms up at the elevated level, so nothing more is reported.
    assert_eq!(
        kinds,
        [
            (FloorEventKind::Rise, None),
            (FloorEventKind::Unknown, None)
        ]
    );
    assert_eq!(events[1].episode, events[0].episode);
    assert_eq!(events[1].onset_seq, 60);
}

#[test]
fn fcme_input_guards_in_the_tracker() {
    let bins = 1024;
    let mut src = GammaFrames::new(bins, 10, provenance(30e6, 1e6), 70);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let flat = vec![1.0f32; bins];
    for _ in 0..20 {
        let frame = src.next(&flat);
        tracker.update(&frame, |_| {});
    }
    let mut frame = src.next(&flat);
    frame.spectrum.psd[..300].fill(0.0);
    frame.spectrum.psd[500] = -f32::NAN;
    frame.spectrum.psd[501] = f32::INFINITY;
    let f = tracker.update(&frame, |_| {});
    assert!(f.valid && f.valid_blocks < 13 && f.valid_blocks > 0);
    assert!(
        f.floor
            .iter()
            .all(|&x| x.is_finite() && (db32(x)).abs() < 0.5),
        "floor collapsed"
    );
    assert!(f.block_iterations.iter().all(|&i| i <= 20));
    let mut frame = src.next(&flat);
    frame.spectrum.psd.fill(0.0);
    let f = tracker.update(&frame, |_| {});
    assert!(!f.valid && f.floor.iter().all(|&x| db32(x).abs() < 0.3));
    assert_eq!(tracker.stats().invalid_frames, 1);
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
            profile[600..650].fill(level * 30.0);
        }
        let frame = src.next(&profile);
        let f = tracker.update(&frame, |e| {
            panic!("{AWARE_006}: drift is not a floor change: {e:?}")
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
}

#[test]
fn per_channel_floors_follow_a_shaped_floor() {
    let bins = 1024;
    let mut src = GammaFrames::new(bins, 10, provenance(446e6, 1e6), 9);
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut profile = vec![1.0f32; bins];
    profile[512..].fill(4.0);
    profile[760..770].fill(400.0);
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
