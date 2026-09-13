//! hackriff DSP kernels: spectral estimation (Welch PSD, STFT, persistence, spectral kurtosis,
//! C07), noise-floor estimation (C08), and the channelizer that feeds dwell zoom streams (C11).
//! Kernels come from rustfft (MIT OR Apache-2.0) on the CPU, liquid-dsp over FFI (MIT) later,
//! and cuFFT/CUDA on the Jetson. A pure-CPU path is the default and is what macOS and CI build.
//! The CUDA path lives behind the `gpu` cargo feature, which is off by default and never
//! enabled by `cargo test --workspace`.
//!
//! Spectral estimation (T-004):
//! - [`window`]: Hann, Blackman-Harris, flat-top with ENBW, coherent gain, scalloping loss.
//! - [`fft`]: the [`FftBackend`] seam and [`CpuFft`]; `gpu::CudaFft` with the `gpu` feature.
//! - [`spectrum`]: [`Spectrum`], units ([`PowerUnit`]), bin geometry and the normalisation
//!   conventions (full-scale complex sinusoid = 0 dBFS), [`Hold`] max/min-hold.
//! - [`welch`]: [`SegmentEngine`] and the one-shot [`welch()`](welch::welch).
//! - [`stft`]: [`StftProcessor`] → [`SpectrumFrame`] over blocks/ring chunks, and
//!   [`DualResolution`].
//! - [`sk`]: the spectral-kurtosis estimator and its variance.
//! - [`persistence`]: the DPX-style decaying histogram.
//! - [`synth`]: deterministic synthetic IQ for tests and benchmarks.
//!
//! Channelization (T-008):
//! - [`filter`]: Kaiser low-pass design with measured response, PFB prototype, SIMD kernels.
//! - [`channelizer`]: the 2× oversampled polyphase filter bank ([`Pfb`], [`PfbBackend`]).
//! - [`ddc`]: the on-demand DDC ([`Ddc`] from a [`DdcSpec`]).

pub mod channelizer;
pub mod ddc;
pub mod fft;
pub mod filter;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod persistence;
pub mod sk;
pub mod spectrum;
pub mod stft;
pub mod synth;
pub mod welch;
pub mod window;

pub use channelizer::{
    ChannelHeader, ChannelSamples, ChannelTime, ChannelizerError, DEFAULT_CHANNEL_RESET_ON, Pfb,
    PfbBackend, PfbConfig, PfbOutput,
};
pub use ddc::{Ddc, DdcBlock, DdcError, DdcPlan, DdcSpec, ResampleKind};
pub use fft::{CpuFft, FftBackend};
pub use filter::{DesignError, FirDesign, LowpassSpec, design_lowpass, pfb_prototype};
pub use persistence::{Persistence, PersistenceConfig};
pub use spectrum::{Hold, HoldKind, PowerUnit, Resolution, Spectrum};
pub use stft::{
    DEFAULT_RESET_ON, DualResolution, InputInfo, IqSample, SpectrumFrame, StftConfig,
    StftProcessor, StftStats, Tier,
};
pub use welch::{ConfigError, SegmentEngine, WelchConfig, welch};
pub use window::{Window, WindowKind, WindowMetrics};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_core() {
        let _ = hk_model::SurveyId::new();
    }
}
