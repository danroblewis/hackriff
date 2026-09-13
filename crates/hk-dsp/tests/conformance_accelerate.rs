//! Conformance suite (ADR-0007) for the Apple Accelerate (vDSP) provider. Compiled only with
//! the `accelerate` feature on macOS. Run with
//! `cargo test -p hk-dsp --features accelerate --test conformance_accelerate -- --nocapture`.
#![cfg(all(feature = "accelerate", target_os = "macos"))]

use hk_dsp::accelerate::{AccelerateFft, AccelerateSpectral};
use hk_dsp::compute::SpectralBackend;
use hk_dsp::conformance::{fft_suite, spectral_suite};
use hk_dsp::{FftBackend, Window};

#[test]
fn accelerate_passes_conformance() {
    fft_suite("accelerate-vdsp", &|n| {
        AccelerateFft::new(n).map(|f| Box::new(f) as Box<dyn FftBackend>)
    })
    .assert_passed();
    spectral_suite("accelerate-vdsp", &|c| {
        let w = Window::new(c.welch.window, c.welch.fft_len);
        AccelerateSpectral::new(&w).map(|s| Box::new(s) as Box<dyn SpectralBackend>)
    })
    .assert_passed();
}
