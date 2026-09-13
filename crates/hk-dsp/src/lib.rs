//! hackriff DSP kernels: spectral estimation (Welch PSD, STFT, persistence, spectral kurtosis,
//! C07), noise-floor estimation (C08), and the channelizer that feeds dwell zoom streams (C11).
//! Kernels come from liquid-dsp over FFI (MIT) and cuFFT/CUDA on the Jetson. A pure-CPU fallback
//! is the default and is what macOS and CI build. The CUDA path lives behind the `gpu` cargo
//! feature, which is off by default and never enabled by `cargo test --workspace`.

#[cfg(feature = "gpu")]
pub mod gpu;

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_core() {
        let _ = hk_model::SurveyId::new();
    }
}
