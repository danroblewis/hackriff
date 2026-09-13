//! GPU polyphase filter bank (Jetson), compiled only with the `gpu` feature.
//!
//! ADR-0007 places the PFB on the Orin GPU. The planned kernels, over unified memory (no host
//! copies):
//! 1. a CUDA kernel that converts the int8 ring samples and computes all `F` folds of a block
//!    at once (`F × L` weighted gathers into slot `absolute index mod M`, with the `(−1)^i`
//!    taps that make FFT bin `c` channel `c`);
//! 2. one batched cuFFT (`cufftPlanMany`, `F × M`) over the folds, in place in the
//!    frame-major output buffer;
//! 3. a gather of the active channels when only a subset is materialised.
//!
//! It implements the same [`PfbBackend`] trait, time map and continuity rules as the CPU
//! [`crate::channelizer::Pfb`], which stays the fallback. No CUDA bindings are linked yet
//! (spike S2 decides them), so [`CudaPfb::new`] always returns [`GpuError::Unavailable`].

use std::convert::Infallible;

use hk_core::Discontinuity;
use num_complex::{Complex, Complex32};

use super::{PfbBackend, PfbConfig, PfbOutput};
use crate::filter::FirDesign;
use crate::gpu::GpuError;
use crate::stft::InputInfo;

/// CUDA PFB (Jetson). Not constructible until the CUDA bindings land.
pub struct CudaPfb {
    /// Uninhabited: proves no instance exists, so the methods are unreachable.
    never: Infallible,
}

impl CudaPfb {
    /// Plans GPU buffers and kernels for `config`. Always [`GpuError::Unavailable`] in this
    /// build; callers fall back to [`crate::channelizer::Pfb`].
    pub fn new(config: &PfbConfig) -> Result<Self, GpuError> {
        let _ = config;
        Err(GpuError::Unavailable(
            "CUDA PFB kernels are not linked yet (spike S2)",
        ))
    }
}

impl PfbBackend for CudaPfb {
    fn name(&self) -> &'static str {
        "gpu-cuda-pfb"
    }

    fn config(&self) -> &PfbConfig {
        match self.never {}
    }

    fn prototype(&self) -> &FirDesign {
        match self.never {}
    }

    fn set_reset_on(&mut self, _flags: Discontinuity) {
        match self.never {}
    }

    fn process_c32(&mut self, _info: InputInfo<'_>, _samples: &[Complex32]) -> PfbOutput<'_> {
        match self.never {}
    }

    fn process_ci8(&mut self, _info: InputInfo<'_>, _samples: &[Complex<i8>]) -> PfbOutput<'_> {
        match self.never {}
    }

    fn reset(&mut self) {
        match self.never {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_unavailable() {
        assert!(matches!(
            CudaPfb::new(&PfbConfig::new(64)),
            Err(GpuError::Unavailable(_))
        ));
    }
}
