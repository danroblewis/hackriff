//! Real-time factor of the T-075 short-burst detector at 2.4, 10 and 20 Msps.
//!
//! Run with `cargo bench -p hk-detect --bench burst_throughput`. Input: 0.5 s of ci8 complex
//! Gaussian noise (−30 dBFS) with a 20 dB squitter-like 120 µs burst every 5 ms (200/s), pushed in
//! 65 536-sample blocks as the detect reader does. Reported: ns per sample and the real-time
//! factor (stream seconds per wall second, one core).

use std::hint::black_box;
use std::time::Instant;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_detect::{BurstConfig, BurstDetector};
use hk_model::{Provenance, SampleTime, SurveyId, Timestamp};
use num_complex::Complex;

fn provenance(fs: f64) -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:bench",
        "tune": {"center_hz": 1090e6, "sample_rate_hz": fs, "lna_db": 24.0, "vga_db": 20.0,
                 "amp_on": false, "bandwidth_hz": 0.75 * fs},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic",
    });
    ProvenanceHandle::new(serde_json::from_value::<Provenance>(json).unwrap())
}

fn input(fs: f64, dur_s: f64) -> Vec<Complex<i8>> {
    let mut state = 0x1234_5678_u64;
    let mut u = move || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (((z ^ (z >> 31)) >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    };
    let n = (fs * dur_s) as usize;
    let sigma = (1e-3 * 128.0 * 128.0 / 2.0f64).sqrt();
    let period = (5e-3 * fs) as usize;
    let burst = (120e-6 * fs) as usize;
    let amp = (100.0 * 1e-3 * 128.0 * 128.0f64).sqrt();
    (0..n)
        .map(|i| {
            let r = (-2.0 * u().ln()).sqrt() * sigma;
            let t = std::f64::consts::TAU * u();
            let on = i % period < burst && (i / 2) % 2 == 0;
            let a = if on { amp } else { 0.0 };
            Complex::new(
                (r * t.cos() + a).round().clamp(-128.0, 127.0) as i8,
                (r * t.sin()).round().clamp(-128.0, 127.0) as i8,
            )
        })
        .collect()
}

fn main() {
    const DUR_S: f64 = 0.5;
    const PASSES: usize = 3;
    for fs in [2.4e6, 10e6, 20e6] {
        let iq = input(fs, DUR_S);
        let prov = provenance(fs);
        let mut best = f64::INFINITY;
        let mut emitted = 0;
        for _ in 0..PASSES {
            let mut det = BurstDetector::new(SurveyId::new(), BurstConfig::default());
            let mut rows = 0usize;
            let t0 = Instant::now();
            for (i, c) in iq.chunks(65_536).enumerate() {
                let s = (i * 65_536) as u64;
                let time = SampleTime {
                    sample_index: s,
                    host_time: Timestamp::UNIX_EPOCH,
                };
                let disc = if i == 0 {
                    Discontinuity::STREAM_START
                } else {
                    Discontinuity::NONE
                };
                det.push(time, disc, &prov, black_box(c), &mut |r| {
                    rows += 1;
                    black_box(r);
                });
            }
            best = best.min(t0.elapsed().as_secs_f64());
            emitted = rows;
        }
        println!(
            "burst_throughput fs {:>5.1} Msps: {:.2} ns/sample, real-time factor {:.1}x (one core), \
             {emitted} bursts in {DUR_S} s",
            fs / 1e6,
            best * 1e9 / iq.len() as f64,
            DUR_S / best
        );
    }
}
