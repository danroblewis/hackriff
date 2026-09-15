//! Chain tests over synthetic signals with hidden ground-truth bits (T-086): 2-FSK (POCSAG
//! rates), MSK 2400 (plus the ACARS AM → 1800 Hz subcarrier path) and RDS (FM → 57 kHz BPSK
//! biphase 1187.5 Bd → clock recovery → differential decoding). ADS-B PPM frames are in
//! `ppm.rs`. Plus an ignored real-time throughput bench.

use std::f64::consts::{FRAC_PI_2, TAU};
use std::time::Instant;

use hk_recipe::PortType;
use num_complex::Complex32;
use serde_json::json;

use super::testkit::*;
use crate::block::PortInfo;
use crate::buffer::PortVec;
use crate::status::Lock;

/// Phase-continuous 2-FSK/MSK at `fs`: ±`dev` around `cfo`, symbol rate `rate·(1 + ppm)`.
fn fsk_iq(
    bits: &[u8],
    fs: f64,
    rate: f64,
    dev: f64,
    cfo: f64,
    noise: f64,
    seed: u64,
) -> Vec<Complex32> {
    let mut rng = Lcg::new(seed);
    let n = (bits.len() as f64 * fs / rate) as usize;
    let mut ph = 0.5;
    (0..n)
        .map(|i| {
            let k = ((i as f64 + 0.5) * rate / fs) as usize;
            let f = cfo
                + if bits[k.min(bits.len() - 1)] == 1 {
                    dev
                } else {
                    -dev
                };
            ph += TAU * f / fs;
            Complex32::new(ph.cos() as f32, ph.sin() as f32) + rng.cnoise(noise)
        })
        .collect()
}

fn random_bits(n: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg::new(seed);
    (0..n).map(|_| rng.bit()).collect()
}

#[test]
fn fsk_1200_with_offset_and_rate_error_recovers_hidden_bits() {
    let truth = random_bits(3_000, 1200);
    let (fs, rate) = (24_000.0, 1_200.0);
    let x = PortVec::Iq(fsk_iq(
        &truth,
        fs,
        rate * (1.0 + 150e-6),
        4_500.0,
        300.0,
        0.05,
        1,
    ));
    let c = assert_chunk_invariant(
        || {
            vec![
                build(
                    "fsk_demod",
                    json!({"deviation_hz": 4500, "offset_tracking_s": 0.5}),
                    PortType::Iq,
                ),
                build(
                    "clock_recovery",
                    json!({"symbol_rate_bd": 1200}),
                    PortType::Real,
                ),
                build("slicer", json!({}), PortType::Soft),
            ]
        },
        PortType::Iq,
        fs,
        &x,
        &[8192, 1000],
    );
    let (errs, n) = bit_errors(&truth, &c.out(2, 0).bits, 100, 8);
    assert!(n > 2_700, "compared {n}");
    assert_eq!(errs, 0, "{errs} errors in {n}");
    let st = c.block(1).status();
    assert_eq!(st.lock, Lock::Locked, "{st:?}");
    let ppm = st.extra.iter().find(|e| e.0 == "rate_ppm").unwrap().1;
    assert!((ppm - 150.0).abs() < 120.0, "rate {ppm} ppm");
    // Soft output time map: one item per symbol.
    let meta = c.out(1, 0).metas[1];
    assert!((meta.source_per_item - fs / rate).abs() < 1e-9);
}

#[test]
fn msk_2400_gardner_and_mueller_muller_recover_hidden_bits() {
    let truth = random_bits(4_000, 2400);
    let fs = 12_000.0;
    let x = PortVec::Iq(fsk_iq(&truth, fs, 2_400.0, 600.0, 0.0, 0.02, 2));
    for algorithm in ["gardner", "mueller-muller"] {
        let c = assert_chunk_invariant(
            || {
                vec![
                    build("msk_demod", json!({"symbol_rate_bd": 2400}), PortType::Iq),
                    build(
                        "clock_recovery",
                        json!({"symbol_rate_bd": 2400, "algorithm": algorithm}),
                        PortType::Real,
                    ),
                    build("slicer", json!({}), PortType::Soft),
                ]
            },
            PortType::Iq,
            fs,
            &x,
            &[4096, 999],
        );
        let (errs, n) = bit_errors(&truth, &c.out(2, 0).bits, 200, 8);
        assert!(n > 3_500, "{algorithm}: compared {n}");
        assert_eq!(errs, 0, "{algorithm}: {errs} errors in {n}");
    }
}

#[test]
fn acars_path_am_subcarrier_msk_recovers_hidden_bits() {
    // An AM carrier keyed by an 1800 Hz MSK tone (±600 Hz, 2400 Bd), as ACARS audio.
    let truth = random_bits(2_400, 131);
    let fs = 24_000.0;
    let mut rng = Lcg::new(4);
    let mut ph = 0.0;
    let x: Vec<Complex32> = (0..(truth.len() * 10))
        .map(|i| {
            let k = i / 10;
            ph += TAU * (1_800.0 + if truth[k] == 1 { 600.0 } else { -600.0 }) / fs;
            let a = 0.3 * (1.0 + 0.6 * ph.cos());
            let c = TAU * 50.0 * i as f64 / fs;
            Complex32::new((a * c.cos()) as f32, (a * c.sin()) as f32) + rng.cnoise(1e-4)
        })
        .collect();
    let c = assert_chunk_invariant(
        || {
            vec![
                build("am_demod", json!({}), PortType::Iq),
                build(
                    "subcarrier",
                    json!({"carrier_hz": 1800, "bandwidth_hz": 2400, "output_rate_hz": 12000}),
                    PortType::Real,
                ),
                build("msk_demod", json!({}), PortType::Iq),
                build(
                    "clock_recovery",
                    json!({"symbol_rate_bd": 2400}),
                    PortType::Real,
                ),
                build("slicer", json!({}), PortType::Soft),
            ]
        },
        PortType::Iq,
        fs,
        &PortVec::Iq(x),
        &[4096, 1234],
    );
    let (errs, n) = bit_errors(&truth, &c.out(4, 0).bits, 200, 12);
    assert!(n > 2_000, "compared {n}");
    assert!(errs * 1000 <= n, "{errs} errors in {n}");
}

/// FM broadcast IQ at 240 kS/s: 1 kHz audio, 19 kHz pilot and RDS (differentially encoded
/// `data`, biphase symbols on 57 kHz in quadrature with the pilot's third harmonic).
fn rds_fm_iq(data: &[u8], seed: u64) -> Vec<Complex32> {
    let fs = 240_000.0;
    let rate = 1_187.5;
    let mut enc = Vec::with_capacity(data.len());
    let mut e = 0u8;
    for &d in data {
        e ^= d;
        enc.push(e);
    }
    let mut rng = Lcg::new(seed);
    let n = (data.len() as f64 * fs / rate) as usize;
    let mut phase = 0.0;
    (0..n)
        .map(|i| {
            let t = i as f64 / fs;
            let pos = t * rate;
            let k = (pos as usize).min(enc.len() - 1);
            let level = if enc[k] == 1 { 1.0 } else { -1.0 };
            let chip = if pos.fract() < 0.5 { level } else { -level };
            let pilot = TAU * 19_000.0 * t + 0.7;
            let mpx = 0.3 * (TAU * 1_000.0 * t).cos()
                + 0.09 * pilot.cos()
                + 0.04 * chip * (3.0 * pilot + FRAC_PI_2).cos();
            phase += TAU * 75_000.0 * mpx / fs;
            Complex32::new(phase.cos() as f32, phase.sin() as f32) + rng.cnoise(1e-3)
        })
        .collect()
}

#[test]
fn rds_fm_subcarrier_biphase_clock_diff_decode_recovers_hidden_bits() {
    let truth = random_bits(2_400, 62);
    let x = PortVec::Iq(rds_fm_iq(&truth, 5));
    let chain = |algorithm: &'static str| {
        move || {
            vec![
                build("fm_demod", json!({"deviation_hz": 75000}), PortType::Iq),
                build(
                    "subcarrier",
                    json!({"carrier_hz": 57000, "bandwidth_hz": 4800, "output_rate_hz": 9500,
                           "reference": {"pilot_hz": 19000, "multiple": 3},
                           "phase_tracking": "bpsk"}),
                    PortType::Real,
                ),
                build(
                    "clock_recovery",
                    json!({"symbol_rate_bd": 1187.5, "pulse": "biphase", "algorithm": algorithm,
                           "soft_from": "in-phase"}),
                    PortType::Iq,
                ),
                build("slicer", json!({"threshold": 0.0}), PortType::Soft),
                build("diff_decode", json!({"mode": "xor"}), PortType::Bits),
            ]
        }
    };
    for algorithm in ["max-contrast", "gardner"] {
        let c = assert_chunk_invariant(
            chain(algorithm),
            PortType::Iq,
            240_000.0,
            &x,
            &[16_384, 5_000],
        );
        let (errs, n) = bit_errors(&truth, &c.out(4, 0).bits, 150, 40);
        assert!(n > 2_000, "{algorithm}: compared {n}");
        assert_eq!(errs, 0, "{algorithm}: {errs} errors in {n}");
        let sc = c.block(1).status();
        assert_eq!(sc.lock, Lock::Locked, "{sc:?}");
        let clock = c.block(2).status();
        assert_eq!(clock.lock, Lock::Locked, "{algorithm}: {clock:?}");
    }
}

#[test]
#[ignore = "timing bench: cargo nextest run -p hk-blocks --release --run-ignored only throughput"]
fn throughput_bench() {
    const N: usize = 1 << 20;
    const CHUNK: usize = 16_384;
    let mut rng = Lcg::new(99);
    let iq: Vec<Complex32> = (0..N)
        .map(|i| Complex32::from_polar(1.0, (i as f32 * 0.3).sin() * 2.0) + rng.cnoise(0.01))
        .collect();
    let real: Vec<f32> = (0..N)
        .map(|i| (i as f32 * 0.37).sin() + 0.1 * rng.gauss() as f32)
        .collect();
    let bits: Vec<u8> = (0..N).map(|_| rng.bit()).collect();
    let cases: Vec<(&str, serde_json::Value, PortType, f64)> = vec![
        ("mix", json!({"offset_hz": 12345}), PortType::Iq, 240e3),
        (
            "lowpass",
            json!({"cutoff_hz": 20000, "transition_hz": 5000}),
            PortType::Iq,
            240e3,
        ),
        (
            "resample",
            json!({"output_rate_hz": 48000}),
            PortType::Iq,
            240e3,
        ),
        (
            "fm_demod",
            json!({"deviation_hz": 75000}),
            PortType::Iq,
            240e3,
        ),
        (
            "fm_demod",
            json!({"deviation_hz": 75000, "output_rate_hz": 48000}),
            PortType::Iq,
            240e3,
        ),
        ("am_demod", json!({}), PortType::Iq, 240e3),
        (
            "fsk_demod",
            json!({"offset_tracking_s": 0.5}),
            PortType::Iq,
            240e3,
        ),
        ("msk_demod", json!({}), PortType::Iq, 240e3),
        (
            "ppm_demod",
            json!({"bit_rate_bd": 1000000, "preamble": "0xA140", "preamble_chips": 16,
            "frame_bits": 112, "length_from": {"offset_bits": 0, "bits": 5,
            "cases": [{"min": 16, "max": 31, "frame_bits": 112}], "default_bits": 56}}),
            PortType::Iq,
            2e6,
        ),
        (
            "subcarrier",
            json!({"carrier_hz": 57000, "bandwidth_hz": 4800, "output_rate_hz": 9500,
            "reference": {"pilot_hz": 19000, "multiple": 3}, "phase_tracking": "bpsk"}),
            PortType::Real,
            240e3,
        ),
        (
            "clock_recovery",
            json!({"symbol_rate_bd": 1200}),
            PortType::Real,
            24e3,
        ),
        (
            "clock_recovery",
            json!({"symbol_rate_bd": 1187.5, "pulse": "biphase", "algorithm": "max-contrast"}),
            PortType::Real,
            9.5e3,
        ),
        (
            "clock_recovery",
            json!({"symbol_rate_bd": 2400, "pulse": "rrc", "algorithm": "mueller-muller"}),
            PortType::Real,
            24e3,
        ),
        ("slicer", json!({}), PortType::Soft, 1e3),
        ("diff_decode", json!({}), PortType::Bits, 1e3),
        ("nrzi", json!({}), PortType::Bits, 1e3),
        ("manchester", json!({}), PortType::Soft, 1e3),
    ];
    eprintln!("{:<16} {:<48} {:>12}", "block", "params", "items/s");
    for (name, p, ty, rate) in cases {
        let data = match ty {
            PortType::Iq => PortVec::Iq(iq.clone()),
            PortType::Real => PortVec::Real(real.clone()),
            PortType::Soft => PortVec::Soft(real.clone()),
            _ => PortVec::Bits(bits.clone()),
        };
        let info = PortInfo {
            ty,
            rate_hz: rate,
            max_items: CHUNK,
            hold_items: 0,
        };
        let mut chain = Chain::new(vec![build(name, p.clone(), ty)], info);
        let t0 = Instant::now();
        chain.run(&data, CHUNK);
        let secs = t0.elapsed().as_secs_f64();
        let ps = p.to_string();
        eprintln!(
            "{name:<16} {:<48} {:>12.3e}",
            &ps[..ps.len().min(48)],
            N as f64 / secs
        );
    }
}
