//! T-008 review fixes for the C11 DDC: a DDC keeps erroring (instead of running a stale plan)
//! until the input rate is realisable again, and its NCO is exact at any stream index. The DDC
//! fronts the SIGNAL-062 / AWARE-036 / SIGNAL-001 chains.

mod common;

use std::f64::consts::PI;

use common::*;
use hk_core::Discontinuity;
use hk_dsp::synth;
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use num_complex::Complex32;

const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;

#[test]
fn ddc_errors_on_every_block_until_the_rate_is_realisable() {
    let prov2 = provenance(433.92e6, 2e6);
    let prov1 = provenance(433.92e6, 1e6);
    let spec = DdcSpec::new(900e3, 100e3);
    let mut ddc = Ddc::new(spec, 2e6).unwrap();
    let delta = 10e3;
    let x = synth::tone(0, 50_000, 900e3 + delta, 2e6, 0.25, 0.0);
    let h = header(0, &prov2, Discontinuity::STREAM_START);
    assert!(
        !ddc.process(InputInfo::from(&h), &x)
            .unwrap()
            .samples
            .is_empty()
    );

    // 900 kHz ± 50 kHz does not fit ±500 kHz: every block at 1 Msps must error.
    let slow = vec![Complex32::new(0.1, 0.0); 25_000];
    for (i, start) in [50_000u64, 75_000].into_iter().enumerate() {
        let h = header(start, &prov1, Discontinuity::NONE);
        let r = ddc.process(InputInfo::from(&h), &slow);
        assert!(
            r.is_err(),
            "block {i} at 1 Msps returned {} samples from a stale 2 Msps plan",
            r.map(|b| b.samples.len()).unwrap_or(0)
        );
    }
    assert_eq!(ddc.plan().input_rate_hz, 2e6);

    // Back at 2 Msps: restarts, reports the lost blocks, and down-converts correctly.
    let start = 100_000u64;
    let x = synth::tone(start, 60_000, 900e3 + delta, 2e6, 0.25, 0.0);
    let h = header(start, &prov2, Discontinuity::NONE);
    let b = ddc.process(InputInfo::from(&h), &x).unwrap();
    let flags = b.header.discontinuity;
    assert!(flags.contains(Discontinuity::RATE_CHANGE), "{flags:?}");
    assert!(flags.contains(Discontinuity::GAP), "{flags:?}");
    assert_eq!(b.header.dropped_before, 50_000);
    assert_eq!(b.header.sample_rate_hz, 200e3);
    let t = b.header.time;
    assert!(t.source_index > start as f64);
    assert!(b.samples.len() > 1000);
    for (k, s) in b.samples.iter().enumerate() {
        let tau = t.source_index_of(k);
        let ph = (2.0 * PI * delta * tau / 2e6).rem_euclid(2.0 * PI);
        let want = Complex32::from_polar(0.5, ph as f32);
        assert!((s - want).norm() < 0.005, "sample {k}: {s} vs {want}");
    }
}

/// `x[n] = A·e^{j2π·frac(n·a)}` with `a = inc / 2^64`, exact at any `n`.
fn exact_tone(start: u64, len: usize, inc: u64, amp: f32) -> Vec<Complex32> {
    (0..len as u64)
        .map(|k| {
            let turns = (start + k).wrapping_mul(inc) as f64 / TWO_POW_64;
            Complex32::from_polar(amp, (2.0 * PI * turns) as f32)
        })
        .collect()
}

#[test]
fn ddc_nco_is_exact_at_large_stream_indices() {
    // Dyadic frequencies so the input tone and the expected baseband are exact at any index:
    // centre 3/16·fs = 375 kHz, tone at 197/1024·fs (offset 5/1024·fs = 9765.625 Hz).
    let fs = 2e6;
    let prov = provenance(433.92e6, fs);
    let spec = DdcSpec::new(375_000.0, 40e3).with_output_rate(50e3);
    let tone_inc = 197u64 << 54;
    let delta_inc = 5u64 << 54;
    let amp = 0.5f32;
    let mut errors = Vec::new();
    for start in [0u64, 72_000_000_000, 1 << 50] {
        let mut ddc = Ddc::new(spec.clone(), fs).unwrap();
        let x = exact_tone(start, 120_000, tone_inc, amp);
        let mut out = Vec::new();
        let mut first = None;
        for (i, chunk) in x.chunks(32_768).enumerate() {
            let flags = if i == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            };
            let h = header(start + (i * 32_768) as u64, &prov, flags);
            let b = ddc.process(InputInfo::from(&h), chunk).unwrap();
            if first.is_none() && !b.samples.is_empty() {
                first = Some(b.header.time);
            }
            out.extend_from_slice(b.samples);
        }
        let t = first.unwrap();
        let mut worst = 0.0f64;
        for (k, s) in out.iter().enumerate() {
            let two_tau = 2.0 * t.source_index_of(k);
            assert_eq!(two_tau.fract(), 0.0, "time map must stay exact at {start}");
            // frac(τ·5/1024) = frac(2τ · (5·2^53) / 2^64).
            let turns = (two_tau as u64).wrapping_mul(delta_inc >> 1) as f64 / TWO_POW_64;
            let want = Complex32::from_polar(amp, (2.0 * PI * turns) as f32);
            worst = worst.max(f64::from((s - want).norm()) / f64::from(amp));
        }
        let err_db = 20.0 * worst.log10();
        eprintln!("DDC NCO at start index {start}: worst error {err_db:.1} dB");
        assert!(err_db < -60.0, "start {start}: {err_db:.1} dB");
        errors.push(err_db);
    }
    let spread = errors.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
        - errors.iter().cloned().fold(f64::INFINITY, f64::min);
    assert!(spread < 1.0, "error depends on stream index: {errors:?}");
}
