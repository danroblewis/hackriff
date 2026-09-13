//! Deterministic synthetic IQ for tests and benchmarks (T1/T4). Not for the real-time path.
//!
//! Power conventions match [`crate::spectrum`]: complex noise of `variance` has
//! `E|x|² = variance`; a tone of `power` has amplitude `sqrt(power)`.

use num_complex::{Complex, Complex32};

/// SplitMix64: small, fast, deterministic.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Seeds the generator.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in (0, 1).
    pub fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// A pair of independent standard normal values (Box–Muller).
    pub fn gaussian_pair(&mut self) -> (f64, f64) {
        let r = (-2.0 * self.unit().ln()).sqrt();
        let th = 2.0 * std::f64::consts::PI * self.unit();
        (r * th.cos(), r * th.sin())
    }
}

/// Circular complex Gaussian noise with `E|x|² = variance`.
pub fn complex_noise(rng: &mut Rng, len: usize, variance: f64) -> Vec<Complex32> {
    let s = (variance / 2.0).sqrt();
    (0..len)
        .map(|_| {
            let (i, q) = rng.gaussian_pair();
            Complex32::new((i * s) as f32, (q * s) as f32)
        })
        .collect()
}

/// A complex tone of `power` (linear, full scale 1) at `offset_hz` from centre. `start` is the
/// stream index of the first sample, so pieces generated separately stay phase-continuous.
pub fn tone(
    start: u64,
    len: usize,
    offset_hz: f64,
    sample_rate_hz: f64,
    power: f64,
    phase: f64,
) -> Vec<Complex32> {
    let a = power.sqrt();
    let w = 2.0 * std::f64::consts::PI * offset_hz / sample_rate_hz;
    (0..len as u64)
        .map(|n| {
            let ph = w * (start + n) as f64 + phase;
            Complex32::new((a * ph.cos()) as f32, (a * ph.sin()) as f32)
        })
        .collect()
}

/// Adds `b` into `a` element-wise.
pub fn add_into(a: &mut [Complex32], b: &[Complex32]) {
    for (x, &y) in a.iter_mut().zip(b) {
        *x += y;
    }
}

/// Quantises to signed 8-bit (`round(x·128)`, clamped to [−128, 127]); returns the samples and
/// the number of clipped components.
pub fn quantize_ci8(samples: &[Complex32]) -> (Vec<Complex<i8>>, usize) {
    let mut clipped = 0;
    let mut q = |v: f32| {
        let x = (v * 128.0).round();
        if !(-128.0..=127.0).contains(&x) {
            clipped += 1;
        }
        x.clamp(-128.0, 127.0) as i8
    };
    let out = samples
        .iter()
        .map(|s| Complex::new(q(s.re), q(s.im)))
        .collect();
    (out, clipped)
}

/// Interleaved ci8 bytes (SigMF `ci8`).
pub fn ci8_bytes(samples: &[Complex<i8>]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|s| [s.re as u8, s.im as u8])
        .collect()
}
