//! AWARE-036, T-011: one named regression test per S5 §4 pitfall (each produced confident wrong
//! answers in an earlier iteration of the spike). Unit-level variants of pitfalls 1, 5 and 6 also
//! live next to the code (`blind::consensus`, `blind::transitions`).

mod blind_support;

use blind_support::*;
use hk_dsp::synth::Rng;
use hk_estimate::blind::transitions::{LsGuards, rate_transitions_ls};
use hk_estimate::blind::{BlindInput, LineGroup, LineMethod, ObwSource};
use hk_estimate::{BlindReason, Family, Reason};
use num_complex::Complex32;

const AWARE_036: &str = "AWARE-036";

/// S5 pitfall 1: |x_c²| is |x|²: a "conjugate-cyclic" line duplicates the envelope line. The estimator has
/// exactly two methods per independent group, and BPSK/QPSK trust needs both groups.
#[test]
fn pitfall1_conjugate_square_magnitude_is_not_an_independent_group() {
    let x = [Complex32::new(0.3, -0.7), Complex32::new(-1.2, 0.4)];
    for v in x {
        assert!(((v * v).norm() - v.norm_sqr()).abs() < 1e-6);
    }
    let groups: Vec<LineGroup> = LineMethod::ALL.iter().map(|m| m.group()).collect();
    assert_eq!(
        groups,
        [
            LineGroup::Envelope,
            LineGroup::Envelope,
            LineGroup::Phase,
            LineGroup::Phase
        ]
    );
    let mut chain = Chain::default();
    for qpsk in [false, true] {
        let fs = 250e3;
        let mut rng = Rng::new(11);
        let (sig, rate) = if qpsk {
            gen_qpsk(&mut rng, 25e3, 500, fs)
        } else {
            gen_bpsk_c(&mut rng, 25e3, 500, fs)
        };
        let obw = clean_obw(&sig, fs);
        let e = embed(&mut rng, &sig, fs, 30.0, obw, 4e-3, 0.0);
        let o = chain.run_embedded(&e, obw);
        let s = o.sym.as_ref().unwrap();
        eprintln!("[{AWARE_036}] qpsk={qpsk}: {}", describe(&o));
        let r = s.symbol_rate_bd.value().expect("trusted");
        assert!((r / rate - 1.0).abs() < 0.01);
        if s.transition_fit.is_none() {
            assert_eq!(
                s.rate_trust.strong_groups,
                [LineGroup::Envelope, LineGroup::Phase],
                "[{AWARE_036}] line-only trust must come from both groups"
            );
        }
    }
}

/// S5 pitfall 2: Global-max line pickers fail on PSK: the |x|² line at Rs sits on a sloped continuum.
/// The whitened picker finds it; the raw periodogram maximum in the same range does not.
#[test]
fn pitfall2_whitening_before_line_picking_on_psk() {
    let fs = 250e3;
    let mut rng = Rng::new(21);
    let (sig, rate) = gen_bpsk_c(&mut rng, 25e3, 500, fs);
    let obw = clean_obw(&sig, fs);
    let e = embed(&mut rng, &sig, fs, 25.0, obw, 4e-3, 0.0);
    let mut chain = Chain::default();
    let o = chain.run_embedded(&e, obw);
    let s = o.sym.as_ref().unwrap();
    let env = s.lines[LineMethod::EnvelopeSquare as usize];
    eprintln!("[{AWARE_036}] {}", describe(&o));
    assert!(
        env.freq_hz.is_some_and(|f| (f / rate - 1.0).abs() < 0.01) && env.significance_db >= 12.0,
        "[{AWARE_036}] whitened |x|² line {env:?}"
    );
    // Unwhitened: raw |x|² periodogram maximum over the same search range.
    let norm = o.norm.as_ref().unwrap();
    let p: Vec<Complex32> = norm
        .samples
        .iter()
        .map(|v| Complex32::new(v.norm_sqr(), 0.0))
        .collect();
    let mean = p.iter().map(|v| v.re).sum::<f32>() / p.len() as f32;
    let p: Vec<Complex32> = p.iter().map(|v| Complex32::new(v.re - mean, 0.0)).collect();
    let nfft = p.len().next_power_of_two() / 2;
    let spec = hk_dsp::welch(
        &p,
        norm.sample_rate_hz,
        0.0,
        &hk_dsp::WelchConfig::new(nfft),
    )
    .unwrap();
    let (lo, hi) = s.rate_range_hz;
    let (mut best_f, mut best_p) = (0.0, 0.0f32);
    for k in 0..spec.bins() {
        let f = spec.bin_offset_hz(k);
        if f >= lo && f <= hi && spec.psd[k] > best_p {
            best_p = spec.psd[k];
            best_f = f;
        }
    }
    eprintln!("[{AWARE_036}] unwhitened global max at {best_f:.0} Hz (Rs {rate:.0})");
    assert!(
        (best_f / rate - 1.0).abs() >= 0.01,
        "[{AWARE_036}] the raw maximum happens to sit at Rs; the regression premise changed"
    );
}

/// S5 pitfall 3: Median-based N0 inflates SNR on noise. C13's mean N0 makes a noise-only box abstain, and
/// C14 without an SNR trusts and labels nothing; even an inflated SNR finds no structure.
#[test]
fn pitfall3_mean_not_median_noise_floor_gates_trust() {
    let fs = 1e6;
    let mut chain = Chain::default();
    for seed in 0..5u64 {
        let mut rng = Rng::new(300 + seed);
        let e = embed(
            &mut rng,
            &vec![Complex32::default(); 30_000],
            fs,
            0.0,
            20e3,
            4e-3,
            0.0,
        );
        let o = chain.run_embedded(&e, 20e3);
        eprintln!("[{AWARE_036}] noise #{seed}: {}", describe(&o));
        assert!(
            o.params.snr_box_db.value().is_none(),
            "mean N0: no SNR on noise"
        );
        assert!(o.trusted_rate().is_none() && o.family() == Family::Unknown);
        // Force a C14 run on the noise with the +9 dB a median N0 gave in S5.
        let snip = &o.snip;
        let input = BlindInput {
            samples: &snip.samples,
            sample_rate_hz: snip.sample_rate_hz,
            obw_hz: 20e3,
            obw_source: ObwSource::Obw99,
            snr_ext_db: Some(9.0),
            noise_power: None,
            channel_bandwidth_hz: 2.0 * snip.passband_hz,
            center_offset_hz: 0.0,
            capture: Some(snip.provenance.get()),
        };
        let s = chain.blind.estimate(&input);
        assert!(
            s.symbol_rate_bd.value().is_none() && s.family == Family::Unknown,
            "[{AWARE_036}] noise with an inflated SNR: {s:?}"
        );
    }
}

/// S5 pitfall 4: NRZ OOK has no |x|² line at Rs (sinc² null): the envelope derivative, run-length seed and
/// transition fit carry it.
#[test]
fn pitfall4_nrz_ook_sinc_null_at_the_rate() {
    let fs = 250e3;
    let mut chain = Chain::default();
    for seed in 0..3u64 {
        let mut rng = Rng::new(40 + seed);
        let sig = gen_ook(&mut rng, 4000.0, 200, fs);
        let obw = clean_obw(&sig, fs);
        let e = embed(&mut rng, &sig, fs, 30.0, obw, 4e-3, 0.0);
        let o = chain.run_embedded(&e, obw);
        let s = o.sym.as_ref().unwrap();
        eprintln!("[{AWARE_036}] OOK #{seed}: {}", describe(&o));
        let env = s.lines[LineMethod::EnvelopeSquare as usize];
        assert!(
            !(env.significance_db >= 12.0
                && env.freq_hz.is_some_and(|f| (f / 4000.0 - 1.0).abs() < 0.01)),
            "[{AWARE_036}] NRZ |x|² line at Rs: {env:?}"
        );
        let r = s
            .symbol_rate_bd
            .value()
            .expect("trusted via the transition fit");
        assert!((r / 4000.0 - 1.0).abs() < 0.01 && s.transition_fit.is_some());
        assert_eq!(s.family, Family::Ook);
    }
}

/// S5 pitfall 5: Outlier rejection hides a wrong clock: a 2.5× seed "fits" by discarding half the
/// transitions. The kept-fraction / odd-run / rise-fall guards refuse it.
#[test]
fn pitfall5_outlier_rejection_hiding_a_wrong_clock() {
    let fs = 250e3;
    let mut rng = Rng::new(51);
    let sig = gen_ook(&mut rng, 4000.0, 300, fs);
    let env: Vec<f64> = sig.iter().map(|v| f64::from(v.re)).collect();
    let t = fs / 4000.0;
    let g = LsGuards::default();
    let right = rate_transitions_ls(&env, fs, 0.5, t * 1.005, None, &g);
    assert!(right.ok, "{right:?}");
    for k in [2.5, 1.5] {
        let wrong = rate_transitions_ls(&env, fs, 0.5, t / k, None, &g);
        eprintln!("[{AWARE_036}] seed {k}× rate: {wrong:?}");
        assert!(
            !(wrong.ok
                && wrong
                    .rate_bd
                    .is_some_and(|r| (r / 4000.0 - 1.0).abs() >= 0.01)),
            "[{AWARE_036}] {k}× seed accepted as a clock: {wrong:?}"
        );
    }
    let open = LsGuards {
        min_kept_fraction: 0.0,
        min_odd_fraction: 0.0,
        max_seed_error: 1.0,
        ..g
    };
    let hidden = rate_transitions_ls(&env, fs, 0.5, t / 2.5, None, &open);
    eprintln!("[{AWARE_036}] 2.5× seed without the guards: {hidden:?}");
}

/// S5 pitfall 6: Harmonics dominate: a long 0101 preamble puts a line at Rs/2. The LS-confirmed candidate
/// outranks it, and ×½ / ×2 are offered.
#[test]
fn pitfall6_harmonic_dominance_ls_outranks_lines() {
    let fs = 250e3;
    let rate = 9600.0;
    let mut chain = Chain::default();
    for seed in 0..3u64 {
        let mut rng = Rng::new(60 + seed);
        let sig = gen_fsk(&mut rng, rate, 4800.0, 240, fs, None, 96);
        let obw = clean_obw(&sig, fs);
        let e = embed(&mut rng, &sig, fs, 30.0, obw, 4e-3, 0.0);
        let o = chain.run_embedded(&e, obw);
        let s = o.sym.as_ref().unwrap();
        eprintln!("[{AWARE_036}] preamble FSK #{seed}: {}", describe(&o));
        let r = s.symbol_rate_bd.value().expect("trusted");
        assert!((r / rate - 1.0).abs() < 0.01, "[{AWARE_036}] {r}");
        assert!(s.candidates[0].transition_fit_ok);
        let alts = &s.harmonic_alternatives_bd;
        assert!(alts.iter().any(|a| (a / (rate / 2.0) - 1.0).abs() < 0.01));
        assert!(alts.iter().any(|a| (a / (rate * 2.0) - 1.0).abs() < 0.01));
    }
}

/// S5 pitfall 7: GFSK h = 0.5 looks like BPSK in x² coherence (MSK-like signals have two x² lines).
#[test]
fn pitfall7_gfsk_h05_is_not_bpsk() {
    let fs = 1e6;
    let mut chain = Chain::default();
    for seed in 0..3u64 {
        let mut rng = Rng::new(70 + seed);
        let sig = gen_fsk(&mut rng, 50e3, 12.5e3, 500, fs, Some(0.5), 32);
        let obw = clean_obw(&sig, fs);
        let e = embed(&mut rng, &sig, fs, 30.0, obw, 2e-3, 0.0);
        let o = chain.run_embedded(&e, obw);
        let s = o.sym.as_ref().unwrap();
        eprintln!(
            "[{AWARE_036}] GFSK #{seed}: c2 {:.2} second x² {:.2}: {}",
            s.family_features.c2,
            s.family_features.x2_second_line,
            describe(&o)
        );
        assert!(s.family_features.x2_second_line >= 0.6 || s.family_features.c2 < 0.25);
        assert!(s.family_scores.bpsk < 0.5);
        assert_ne!(s.family, Family::Bpsk);
    }
}

/// S5 pitfall 8: Chirps and FM pilots fool IF bimodality and LS (periodic decisions / one run length).
#[test]
fn pitfall8_chirp_and_pilot_periodicity() {
    let fs = 1e6;
    let mut chain = Chain::default();
    for seed in 0..3u64 {
        let mut rng = Rng::new(80 + seed);
        let chirp = gen_chirp(125e3, 7, 8, fs);
        let e = embed(&mut rng, &chirp, fs, 30.0, 125e3, 4e-3, 0.0);
        let o = chain.run_embedded(&e, 125e3);
        eprintln!("[{AWARE_036}] chirp #{seed}: {}", describe(&o));
        assert!(o.trusted_rate().is_none() && o.family() == Family::Unknown);
        // A pilot-like tone modulated by a slow sine (FM, 19 kHz-style periodic IF).
        let n = 40_000;
        let mut ph = 0.0f64;
        let tone: Vec<Complex32> = (0..n)
            .map(|k| {
                ph += std::f64::consts::TAU
                    * 6_000.0
                    * (std::f64::consts::TAU * 1_900.0 * k as f64 / fs).sin()
                    / fs;
                Complex32::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let obw = clean_obw(&tone, fs);
        let e = embed(&mut rng, &tone, fs, 30.0, obw, 4e-3, 0.0);
        let o = chain.run_embedded(&e, obw);
        eprintln!("[{AWARE_036}] periodic FM #{seed}: {}", describe(&o));
        assert!(
            o.trusted_rate().is_none(),
            "[{AWARE_036}] periodic IF trusted"
        );
        assert_eq!(o.family(), Family::Unknown);
    }
}

/// S5 pitfall 9: Multi-frame detector boxes (hop edges, ack gaps) mask lines: the fit splits segments with
/// their own offsets, and the envelope picks the longest on-segment when frames are few.
#[test]
fn pitfall9_multi_segment_boxes() {
    let fs = 250e3;
    let rate = 9600.0;
    let mut chain = Chain::default();
    for seed in 0..3u64 {
        let mut rng = Rng::new(90 + seed);
        let a = gen_fsk(&mut rng, rate, 4800.0, 300, fs, None, 32);
        let b = gen_fsk(&mut rng, rate, 4800.0, 200, fs, None, 32);
        // A gap of 150.37 symbols: the second frame is off the first frame's lattice.
        let gap = (150.37 * fs / rate) as usize;
        let mut sig = a.clone();
        sig.extend(std::iter::repeat_n(Complex32::default(), gap));
        sig.extend(&b);
        let obw = clean_obw(&a, fs);
        let e = embed(&mut rng, &sig, fs, 25.0, obw, 4e-3, 0.0);
        let o = chain.run_embedded(&e, obw);
        let s = o.sym.as_ref().unwrap();
        eprintln!("[{AWARE_036}] two frames #{seed}: {}", describe(&o));
        let r = s.symbol_rate_bd.value().expect("trusted");
        assert!((r / rate - 1.0).abs() < 0.01);
        let fit = s.transition_fit.as_ref().unwrap();
        assert!(
            fit.segments >= 2 || s.reasons.contains(&BlindReason::MultiSegment),
            "[{AWARE_036}] {fit:?}"
        );
        let d = s.deviation_hz.value().expect("deviation");
        assert!(
            (d / 4800.0 - 1.0).abs() < 0.10,
            "[{AWARE_036}] deviation {d}"
        );
    }
}

/// S5 pitfall 10: Rate trust ≠ family trust: lines integrate over time (a long BPSK stream at 3 dB has a
/// trusted rate) but family features need ≥ 8 dB: `unknown` with `low_snr`.
#[test]
fn pitfall10_rate_trust_is_not_family_trust() {
    let fs = 250e3;
    let mut rng = Rng::new(100);
    let (sig, rate) = gen_bpsk_c(&mut rng, 25e3, 6000, fs);
    let obw = clean_obw(&sig, fs);
    let e = embed(&mut rng, &sig, fs, 3.0, obw, 4e-3, 0.0);
    let mut chain = Chain::default();
    let o = chain.run_embedded(&e, obw);
    eprintln!(
        "[{AWARE_036}] long BPSK at 3 dB: {} | C13 obw {:?} snr {:?}",
        describe(&o),
        o.params.obw99_hz,
        o.params.snr_extent_db
    );
    let s = o.sym.as_ref().expect("C13 measured OBW");
    let r = s.symbol_rate_bd.value().expect("rate trusted at 3 dB");
    assert!((r / rate - 1.0).abs() < 0.01);
    assert_eq!(s.family, Family::Unknown);
    assert!(s.reasons.contains(&BlindReason::LowSnr));
    assert_eq!(s.deviation_hz.reason(), Some(Reason::NotApplicable));
}
