//! A small seeded PRNG (xoshiro256** seeded through SplitMix64). No external dependency, so a
//! seed reproduces a run bit for bit on the same platform.

/// SplitMix64 finaliser: diffuses one 64-bit value.
pub fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// xoshiro256** generator.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// A generator for `seed`.
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            mix64(x)
        };
        Self {
            s: [next(), next(), next(), next()],
        }
    }

    /// An independent stream `stream` of `seed` (e.g. one per emitter).
    pub fn derive(seed: u64, stream: u64) -> Self {
        Self::new(mix64(seed) ^ mix64(stream.wrapping_add(0x5851_F42D_4C95_7F2D)))
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in `[0, 1)`.
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in `[lo, hi)` (`lo` when the range is empty).
    pub fn uniform(&mut self, lo: f64, hi: f64) -> f64 {
        if hi <= lo {
            lo
        } else {
            lo + (hi - lo) * self.f64()
        }
    }

    /// Log-uniform in `[lo, hi)`, both > 0.
    pub fn log_uniform(&mut self, lo: f64, hi: f64) -> f64 {
        self.uniform(lo.ln(), hi.ln()).exp()
    }

    /// Uniform integer in `[0, n)`; 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            ((self.f64() * n as f64) as u64).min(n - 1)
        }
    }

    /// Exponential with the given mean.
    pub fn exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.f64()).ln()
    }

    /// Standard normal (Box–Muller).
    pub fn normal(&mut self) -> f64 {
        let u1 = 1.0 - self.f64();
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// One element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream_and_streams_differ() {
        let a: Vec<u64> = (0..8).map(|_| Rng::new(7).next_u64()).collect();
        assert!(a.windows(2).all(|w| w[0] == w[1]));
        let mut x = Rng::derive(7, 1);
        let mut y = Rng::derive(7, 2);
        assert_ne!(x.next_u64(), y.next_u64());
    }

    #[test]
    fn uniform_and_exp_means() {
        let mut r = Rng::new(42);
        let n = 200_000;
        let mean_u = (0..n).map(|_| r.f64()).sum::<f64>() / n as f64;
        let mean_e = (0..n).map(|_| r.exp(3.0)).sum::<f64>() / n as f64;
        assert!((mean_u - 0.5).abs() < 0.005, "{mean_u}");
        assert!((mean_e - 3.0).abs() < 0.03, "{mean_e}");
    }
}
