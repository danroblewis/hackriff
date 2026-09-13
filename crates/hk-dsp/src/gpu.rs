//! GPU kernels (cuFFT/CUDA), compiled only with the `gpu` feature.
//!
//! CUDA kernels build on the Jetson only (`just deploy-jetson` runs
//! `cargo build --release --features hk-dsp/gpu` on the device, where JetPack provides CUDA).
//! macOS and CI never enable this feature and use the CPU path.
//!
//! [`CudaFft`] is the cuFFT [`FftBackend`] placeholder. The Jetson implementation will plan a
//! batched `cufftPlanMany` over unified memory and override
//! [`FftBackend::forward_batch`] so a whole frame of segments is one kernel launch (ADR-0007:
//! wideband FFT, persistence and spectral kurtosis on the GPU). No CUDA bindings are linked
//! yet (spike S2 decides the binding), so [`CudaFft::new`] always returns
//! [`GpuError::Unavailable`] and callers fall back to [`crate::fft::CpuFft`].

use std::convert::Infallible;
use std::fmt;

use num_complex::Complex32;

use crate::fft::FftBackend;

/// Why a GPU backend could not be created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GpuError {
    /// No usable CUDA runtime or bindings in this build.
    Unavailable(&'static str),
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuError::Unavailable(why) => write!(f, "GPU FFT unavailable: {why}"),
        }
    }
}

impl std::error::Error for GpuError {}

/// cuFFT forward FFT (Jetson). Not constructible until the CUDA bindings land.
pub struct CudaFft {
    len: usize,
    /// Uninhabited: proves no instance exists, so the transform methods are unreachable.
    never: Infallible,
}

impl CudaFft {
    /// Plans a cuFFT forward transform of length `len`. Always
    /// [`GpuError::Unavailable`] in this build.
    pub fn new(len: usize) -> Result<Self, GpuError> {
        let _ = len;
        Err(GpuError::Unavailable(
            "cuFFT bindings are not linked yet (spike S2)",
        ))
    }
}

impl FftBackend for CudaFft {
    fn len(&self) -> usize {
        self.len
    }

    fn name(&self) -> &'static str {
        "gpu-cufft"
    }

    fn forward(&mut self, _buf: &mut [Complex32]) {
        match self.never {}
    }

    fn forward_batch(&mut self, _buf: &mut [Complex32]) {
        match self.never {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_unavailable() {
        assert!(matches!(CudaFft::new(4096), Err(GpuError::Unavailable(_))));
    }
}
