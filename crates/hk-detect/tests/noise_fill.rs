//! T-237: heavily quantised noise (the mock device's noise fill, and any real receiver run far
//! below its recording's gain) through the real chain: ci8 IQ → STFT → floor tracker → detector.
//!
//! A near-full-window box on flat noise is the symptom T-231 saw once in 40 loaded t057 runs
//! (2.8125 MHz occupied of a 3 Msps window = 15/16 of the span). These tests reproduce the class
//! deterministically and cheaply by sweeping the code-domain noise level.

mod common;

use common::*;
use hk_core::{BlockHeader, Discontinuity};
use hk_detect::{ClipCount, Detector, DetectorConfig, DetectorEvent, count_clipped_ci8};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker, gamma};
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_model::{SampleTime, SurveyId, Timestamp};
use num_complex::Complex;

/// Complex Gaussian noise with per-component standard deviation `sigma_codes`, rounded to ci8 as
/// an ADC does. Returns the samples and the fraction of **codes** (I and Q separately) that are 0.
fn noise_ci8(n: usize, sigma_codes: f64, seed: u64) -> (Vec<Complex<i8>>, f64) {
    let mut rng = Rng(seed);
    let mut v = Vec::with_capacity(n);
    let mut zeros = 0usize;
    let q = |x: f64| x.round().clamp(-128.0, 127.0) as i8;
    for _ in 0..n {
        let (u1, u2) = (rng.unit(), rng.unit());
        let r = (-2.0 * u1.ln()).sqrt() * sigma_codes;
        let a = std::f64::consts::TAU * u2;
        let (re, im) = (q(r * a.cos()), q(r * a.sin()));
        zeros += usize::from(re == 0) + usize::from(im == 0);
        v.push(Complex::new(re, im));
    }
    (v, zeros as f64 / (2 * n) as f64)
}

/// What one sweep point measured.
#[derive(Debug, Default)]
struct Report {
    frames: u64,
    boxes: usize,
    /// Boxes spanning at least half the bins.
    wide_boxes: usize,
    /// Widest box as a fraction of the bins.
    max_span: f64,
    /// Occupied bandwidth of the widest box, Hz.
    max_obw_hz: f64,
    /// Median over frames of (median PSD / median floor-branch reference), dB.
    median_excess_db: f64,
    /// Fraction of cells the CFAR called `region`, over all frames.
    region_fraction: f64,
    /// Frames the floor tracker flagged quantisation-limited.
    quantisation_frames: u64,
    /// Frames the floor tracker called invalid.
    invalid_frames: u64,
    /// Mean effective averages the tracker reported.
    n_avg: f64,
}

/// Runs `sigma_codes` noise through STFT → floor tracker → detector at one geometry.
fn sweep_point(fs: f64, fft_len: usize, averages: usize, sigma_codes: f64, seed: u64) -> Report {
    let samples_needed = fft_len * averages * 400;
    let (iq, zero_fraction) = noise_ci8(samples_needed, sigma_codes, seed);
    let prov = provenance_full(98e6, fs, 24.0, fs * 0.75, false, false);
    let welch = WelchConfig {
        fft_len,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, averages)).expect("stft");
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).expect("floor");
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).expect("detector");
    let mut r = Report::default();
    let mut excess = Vec::new();
    let (mut region, mut cells) = (0u64, 0u64);
    let (mut n_avg_sum, mut n_avg_count) = (0.0f64, 0u64);
    let record = |e: DetectorEvent<'_>, r: &mut Report| {
        if let DetectorEvent::Detection(d) = e {
            r.boxes += 1;
            let span = (d.bins.end - d.bins.start) as f64 / fft_len as f64;
            if span >= 0.5 {
                r.wide_boxes += 1;
            }
            if span > r.max_span {
                r.max_span = span;
                r.max_obw_hz = d.detection.obw_hz;
            }
        }
    };
    let mut s = 0usize;
    while s < iq.len() {
        let e = (s + 65_536).min(iq.len());
        let header = BlockHeader {
            time: SampleTime {
                sample_index: s as u64,
                host_time: Timestamp::from_unix_nanos((s as f64 * 1e9 / fs).round() as i64),
            },
            provenance: prov.clone(),
            discontinuity: if s == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
        };
        stft.push(InputInfo::from(&header), &iq[s..e], |frame| {
            let f = tracker.update(frame, |_| {});
            n_avg_sum += f.n_avg_effective;
            n_avg_count += 1;
            r.quantisation_frames += u64::from(f.quantisation_limited);
            r.invalid_frames += u64::from(!f.valid);
            let a = frame.t.sample_index as usize;
            let b = (a + frame.sample_count as usize).min(iq.len());
            let clip = ClipCount::new(count_clipped_ci8(&iq[a..b]), (b - a) as u64);
            det.process(frame, f, clip, &mut |ev| record(ev, &mut r));
            // The reference the floor branch actually used, against the frame's own PSD.
            let reference = det.floor_branch_reference();
            let mut p: Vec<f32> = frame.spectrum.psd.clone();
            let mut q: Vec<f32> = reference.to_vec();
            p.sort_by(f32::total_cmp);
            q.sort_by(f32::total_cmp);
            let (mp, mq) = (p[p.len() / 2], q[q.len() / 2]);
            if mq > 0.0 {
                excess.push(10.0 * f64::from(mp / mq).log10());
            }
            let st = det.last_classify();
            region += st.region as u64;
            cells += fft_len as u64;
            r.frames += 1;
        });
        s = e;
    }
    det.finish(&mut |ev| record(ev, &mut r));
    excess.sort_by(f64::total_cmp);
    r.median_excess_db = excess.get(excess.len() / 2).copied().unwrap_or(0.0);
    r.region_fraction = region as f64 / cells.max(1) as f64;
    r.n_avg = n_avg_sum / n_avg_count.max(1) as f64;
    eprintln!(
        "sigma {sigma_codes:>5.2} codes ({:.1} % zero codes): {} frames, n_avg {:.1}, \
         {} boxes ({} spanning >= half the window, widest {:.3} of the span = {:.0} kHz obw); \
         median PSD/reference {:+.2} dB, region cells {:.2e}, quantisation-limited frames {}, \
         invalid {}; floor-branch T_on {:.2} dB",
        zero_fraction * 100.0,
        r.frames,
        r.n_avg,
        r.boxes,
        r.wide_boxes,
        r.max_span,
        r.max_obw_hz / 1e3,
        r.median_excess_db,
        r.region_fraction,
        r.quantisation_frames,
        r.invalid_frames,
        10.0 * gamma::mean_threshold(r.n_avg.max(1.0), 1e-6).log10(),
    );
    r
}

/// Instrumentation sweep: how the chain behaves as the noise approaches the quantiser.
#[test]
fn quantised_noise_sweep_shows_where_whole_window_boxes_appear() {
    // The t057 geometry of the window T-231 saw: 3 Msps, 512 bins, 10 averages.
    for &sigma in &[0.25f64, 0.4, 0.55, 0.7, 1.0, 1.5, 3.0, 8.0] {
        let r = sweep_point(3e6, 512, 10, sigma, 0x7057u64 ^ sigma.to_bits());
        assert_eq!(
            (r.boxes, r.wide_boxes),
            (0, 0),
            "stationary quantised noise at sigma {sigma} codes produced boxes"
        );
    }
}

/// What one transition produced.
#[derive(Debug, Default)]
struct StepReport {
    boxes: usize,
    wide_boxes: usize,
    impulsive_boxes: usize,
    max_span: f64,
    max_obw_hz: f64,
}

/// `pre` frames of noise at `sigma_a`, then the same count at `sigma_b`, with `flags` and a
/// (possibly retuned) provenance on the first block of the second half — the mock's retune.
#[allow(clippy::too_many_arguments)]
fn transition(
    fs: f64,
    fft_len: usize,
    averages: usize,
    sigma_a: f64,
    sigma_b: f64,
    flags: Discontinuity,
    retune_hz: f64,
    label: &str,
) -> StepReport {
    let frames_each = 60usize;
    let per = fft_len * averages * frames_each;
    let (mut iq, _) = noise_ci8(per, sigma_a, 0xa11ce);
    let (tail, _) = noise_ci8(per, sigma_b, 0xb0b);
    iq.extend_from_slice(&tail);
    let prov_a = provenance_full(98e6, fs, 24.0, fs * 0.75, false, false);
    let prov_b = provenance_full(98e6 + retune_hz, fs, 24.0, fs * 0.75, false, false);
    let welch = WelchConfig {
        fft_len,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, averages)).expect("stft");
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).expect("floor");
    let mut det = Detector::new(DetectorConfig::new(SurveyId::new())).expect("detector");
    let mut r = StepReport::default();
    let mut frame_no = 0u64;
    let record = |e: DetectorEvent<'_>, r: &mut StepReport| {
        if let DetectorEvent::Detection(d) = e {
            r.boxes += 1;
            let span = (d.bins.end - d.bins.start) as f64 / fft_len as f64;
            if span >= 0.5 {
                r.wide_boxes += 1;
                eprintln!(
                    "  [{label}] BOX span {:.4} of the window, obw {:.1} kHz, bins {}..{}, \
                     snr {:.1}/{:.1} dB, impulsive {}, marginal {}, edge {}",
                    span,
                    d.detection.obw_hz / 1e3,
                    d.bins.start,
                    d.bins.end,
                    d.detection.snr_peak_db,
                    d.detection.snr_mean_db,
                    d.detection.flags.impulsive,
                    d.detection.flags.marginal,
                    d.detection.flags.edge,
                );
            }
            if d.detection.flags.impulsive {
                r.impulsive_boxes += 1;
            }
            if span > r.max_span {
                r.max_span = span;
                r.max_obw_hz = d.detection.obw_hz;
            }
        }
    };
    let mut s = 0usize;
    while s < iq.len() {
        let e = (s + 65_536).min(iq.len());
        // The second half starts exactly on a block boundary.
        let e = if s < per && e > per { per } else { e };
        let second = s >= per;
        let header = BlockHeader {
            time: SampleTime {
                sample_index: s as u64,
                host_time: Timestamp::from_unix_nanos((s as f64 * 1e9 / fs).round() as i64),
            },
            provenance: if second {
                prov_b.clone()
            } else {
                prov_a.clone()
            },
            discontinuity: if s == 0 {
                Discontinuity::STREAM_START
            } else if s == per {
                flags
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
        };
        stft.push(InputInfo::from(&header), &iq[s..e], |frame| {
            let f = tracker.update(frame, |_| {});
            let band_floor = f.band_floor;
            let (impulsive, excess, valid, seg) =
                (f.impulsive, f.impulsive_excess_db, f.valid, f.segment);
            let a = frame.t.sample_index as usize;
            let b = (a + frame.sample_count as usize).min(iq.len());
            let clip = ClipCount::new(count_clipped_ci8(&iq[a..b]), (b - a) as u64);
            det.process(frame, f, clip, &mut |ev| record(ev, &mut r));
            let reference = det.floor_branch_reference();
            let mut p: Vec<f32> = frame.spectrum.psd.clone();
            let mut q: Vec<f32> = reference.to_vec();
            p.sort_by(f32::total_cmp);
            q.sort_by(f32::total_cmp);
            let (mp, mq) = (p[p.len() / 2], q[q.len() / 2]);
            let st = det.last_classify();
            // The frames around the step are the interesting ones.
            if (frames_each as u64).saturating_sub(4) <= frame_no
                && frame_no <= frames_each as u64 + 8
            {
                eprintln!(
                    "  [{label}] frame {frame_no:>3} seg {seg} floor-seg-valid {valid} \
                     impulsive {impulsive} (+{excess:.2} dB) band_floor {band_floor:.3e} \
                     median PSD {mp:.3e} reference {mq:.3e} PSD/ref {:+.2} dB \
                     seeds {} region {}",
                    10.0 * f64::from(mp / mq.max(1e-37)).log10(),
                    st.seeds,
                    st.region,
                );
            }
            frame_no += 1;
        });
        s = e;
    }
    det.finish(&mut |ev| record(ev, &mut r));
    eprintln!(
        "[{label}] sigma {sigma_a} -> {sigma_b}: {} boxes, {} spanning >= half the window \
         ({} impulsive), widest {:.4} of the span = {:.0} kHz obw",
        r.boxes,
        r.wide_boxes,
        r.impulsive_boxes,
        r.max_span,
        r.max_obw_hz / 1e3,
    );
    r
}

/// A level step with no floor reset is the non-stationary case the stationary sweep cannot show.
#[test]
fn level_steps_across_a_retune_do_not_make_whole_window_boxes() {
    let quiet = 0.7;
    let loud = 0.7 * 10f64.powf(12.0 / 20.0);
    // A retune (new provenance, new centre) as the mock does it, both directions.
    transition(
        3e6,
        512,
        10,
        quiet,
        loud,
        Discontinuity::PROVENANCE_CHANGE,
        1.2e6,
        "retune-up",
    );
    transition(
        3e6,
        512,
        10,
        loud,
        quiet,
        Discontinuity::PROVENANCE_CHANGE,
        1.2e6,
        "retune-down",
    );
    // A level step at constant tune (a coverage change): PROVENANCE_CHANGE is not in
    // FLOOR_RESET_ON and the gain key is unchanged, so the floor tracker does not reset.
    let up = transition(
        3e6,
        512,
        10,
        quiet,
        loud,
        Discontinuity::PROVENANCE_CHANGE,
        0.0,
        "coverage-up",
    );
    let down = transition(
        3e6,
        512,
        10,
        loud,
        quiet,
        Discontinuity::PROVENANCE_CHANGE,
        0.0,
        "coverage-down",
    );
    assert_eq!(
        (up.wide_boxes, down.wide_boxes),
        (0, 0),
        "a level step made a near-whole-window box"
    );
}
