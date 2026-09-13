//! FFT backends.
//!
//! [`FftBackend`] is the seam between the spectral estimators and the transform. The CPU
//! backend ([`CpuFft`], rustfft, NEON on aarch64 / AVX on x86_64) is the default and is what
//! macOS and CI build. The Jetson backend (`gpu::CudaFft`, cuFFT) is compiled only with the
//! `gpu` cargo feature.
//!
//! Transforms are forward, unnormalised: `X[k] = Σ x[n]·e^{−j2πkn/N}`, in FFT order (DC at
//! index 0). Normalisation and DC-centring are the caller's job ([`crate::spectrum`]).

use std::sync::Arc;

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// A planned forward FFT of a fixed length. Implementations own their scratch space, so steady
/// state calls never allocate.
pub trait FftBackend: Send {
    /// Transform length `N`.
    fn len(&self) -> usize;

    /// Always false: a planned transform has `N >= 1`.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A short name for logs and benchmark output.
    fn name(&self) -> &'static str;

    /// In-place forward FFT of one segment; `buf.len() == self.len()`.
    fn forward(&mut self, buf: &mut [Complex32]);

    /// In-place forward FFT of `buf.len() / N` concatenated segments. The default loops over
    /// [`FftBackend::forward`]; a GPU backend overrides it to transform a whole batch in one
    /// kernel launch.
    fn forward_batch(&mut self, buf: &mut [Complex32]) {
        let n = self.len();
        assert_eq!(buf.len() % n, 0, "batch is not a whole number of segments");
        for seg in buf.chunks_exact_mut(n) {
            self.forward(seg);
        }
    }
}

/// CPU FFT over rustfft (MIT OR Apache-2.0) with a reused scratch buffer.
pub struct CpuFft {
    plan: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
}

impl CpuFft {
    /// Plans a forward FFT of length `len` (> 0). Planning allocates; `forward` does not.
    pub fn new(len: usize) -> Self {
        assert!(len > 0, "FFT length must be positive");
        let plan = FftPlanner::<f32>::new().plan_fft_forward(len);
        let scratch = vec![Complex32::default(); plan.get_inplace_scratch_len()];
        Self { plan, scratch }
    }
}

impl FftBackend for CpuFft {
    fn len(&self) -> usize {
        self.plan.len()
    }

    fn name(&self) -> &'static str {
        "cpu-rustfft"
    }

    #[inline]
    fn forward(&mut self, buf: &mut [Complex32]) {
        self.plan.process_with_scratch(buf, &mut self.scratch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_direct_dft() {
        let n = 16;
        let x: Vec<Complex32> = (0..n)
            .map(|i| Complex32::new((i as f32 * 0.7).sin(), (i as f32 * 0.3).cos()))
            .collect();
        let mut buf = x.clone();
        CpuFft::new(n).forward(&mut buf);
        for (k, got) in buf.iter().enumerate() {
            let want: Complex32 = x
                .iter()
                .enumerate()
                .map(|(i, &v)| {
                    let phi = -2.0 * std::f32::consts::PI * (k * i) as f32 / n as f32;
                    v * Complex32::from_polar(1.0, phi)
                })
                .sum();
            assert!((got - want).norm() < 1e-4, "bin {k}: {got} vs {want}");
        }
    }
}
