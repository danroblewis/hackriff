//! Shared synthetic HackRF-like IQ source material (spike S1).
//!
//! A table of interleaved int8 I/Q (the HackRF wire format) holding four FM
//! carriers plus Gaussian noise at 20 Msps. Every carrier and modulating tone
//! sits on the fs/LOOP_LEN grid, so looping the table is phase-continuous and
//! the demodulated tones have no loop glitch.
//!
//! Both implementations (FutureSDR and owned) include this file with
//! `#[path]`, so they process identical samples.

#![allow(dead_code)]

use std::f64::consts::TAU;

/// Complex sample rate (HackRF One maximum).
pub const FS: f64 = 20_000_000.0;
/// Samples per block / buffer chunk: 65 536 samples = 3.2768 ms at 20 Msps.
/// This is "one buffer period" for the pass/fail criterion.
pub const BLOCK: usize = 65_536;
/// Samples in the looped table (≈0.21 s).
pub const LOOP_LEN: usize = 1 << 22;
/// HackRF-like host FIFO model: a source that falls further behind real time
/// than this many samples drops the excess (one libhackrf USB transfer is
/// 262 144 bytes = 131 072 samples).
pub const FIFO_CAP: u64 = 131_072;

#[derive(Clone, Copy, Debug)]
pub struct Carrier {
    pub offset_hz: f64,
    pub dev_hz: f64,
    pub audio_hz: f64,
    pub amp: f64,
}

/// Snap a frequency to the loop grid so the table repeats seamlessly.
pub fn snap(f: f64) -> f64 {
    let q = FS / LOOP_LEN as f64;
    (f / q).round() * q
}

pub fn carriers() -> Vec<Carrier> {
    let c = |offset: f64, dev: f64, audio: f64, amp: f64| Carrier {
        offset_hz: snap(offset),
        dev_hz: dev,
        audio_hz: snap(audio),
        amp,
    };
    vec![
        c(-6_000_000.0, 5_000.0, 800.0, 0.15),
        c(-1_500_000.0, 5_000.0, 1_000.0, 0.20), // NBFM chain target
        c(2_500_000.0, 75_000.0, 1_700.0, 0.20), // WBFM chain target
        c(7_000_000.0, 12_500.0, 2_000.0, 0.15),
    ]
}

/// Build the looped int8 interleaved table (2 * LOOP_LEN bytes).
pub fn table_i8() -> Vec<i8> {
    let carriers = carriers();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let noise_sigma = 0.03;
    let mut out = Vec::with_capacity(LOOP_LEN * 2);
    for n in 0..LOOP_LEN {
        let t = n as f64 / FS;
        let (mut i, mut q) = (0.0f64, 0.0f64);
        for c in &carriers {
            let beta = c.dev_hz / c.audio_hz;
            let ph = TAU * c.offset_hz * t + beta * (TAU * c.audio_hz * t).sin();
            i += c.amp * ph.cos();
            q += c.amp * ph.sin();
        }
        let (ni, nq) = rng.gauss2();
        i += noise_sigma * ni;
        q += noise_sigma * nq;
        out.push((i * 127.0).round().clamp(-127.0, 127.0) as i8);
        out.push((q * 127.0).round().clamp(-127.0, 127.0) as i8);
    }
    out
}

struct XorShift(u64);
impl XorShift {
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss2(&mut self) -> (f64, f64) {
        let u1 = self.next_f64().max(1e-12);
        let u2 = self.next_f64();
        let r = (-2.0 * u1.ln()).sqrt();
        (r * (TAU * u2).cos(), r * (TAU * u2).sin())
    }
}
