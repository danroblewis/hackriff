//! GPU kernels (cuFFT/CUDA), compiled only with the `gpu` feature.
//!
//! CUDA kernels build on the Jetson only (`just deploy-jetson` runs
//! `cargo build --release --features hk-dsp/gpu` on the device, where JetPack provides CUDA).
//! macOS and CI never enable this feature and use the CPU path. Empty stub until T-004.
