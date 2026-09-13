//! Conformance suite (ADR-0007) for the wgpu GPU provider. Compiled only with `gpu-wgpu`;
//! skips with a logged reason when no GPU adapter is present. Run locally with
//! `cargo test -p hk-dsp --features gpu-wgpu --test conformance_gpu -- --nocapture`.
#![cfg(feature = "gpu-wgpu")]

use hk_dsp::channelizer::batch::BatchPfb;
use hk_dsp::compute::SpectralBackend;
use hk_dsp::conformance::{fft_suite, pfb_suite, spectral_suite};
use hk_dsp::gpu_wgpu::{self, WgpuFft, WgpuPfbExecutor, WgpuSpectral};
use hk_dsp::{FftBackend, PfbBackend, Window};

fn context_or_skip() -> Option<std::sync::Arc<gpu_wgpu::GpuContext>> {
    match gpu_wgpu::context() {
        Ok(ctx) => {
            eprintln!("gpu-wgpu conformance on {}", ctx.describe());
            Some(ctx)
        }
        Err(why) => {
            eprintln!("SKIP gpu-wgpu conformance: {why}");
            None
        }
    }
}

#[test]
fn wgpu_fft_passes_conformance() {
    let Some(ctx) = context_or_skip() else { return };
    fft_suite("gpu-wgpu", &|n| {
        WgpuFft::new(ctx.clone(), n).map(|f| Box::new(f) as Box<dyn FftBackend>)
    })
    .assert_passed();
}

#[test]
fn wgpu_spectral_passes_conformance() {
    let Some(ctx) = context_or_skip() else { return };
    for in_flight in [0usize, 2] {
        spectral_suite(&format!("gpu-wgpu (in-flight {in_flight})"), &|c| {
            let w = Window::new(c.welch.window, c.welch.fft_len);
            Ok(
                Box::new(WgpuSpectral::new(ctx.clone(), &w, c.welch.hop(), in_flight))
                    as Box<dyn SpectralBackend>,
            )
        })
        .assert_passed();
    }
}

#[test]
fn wgpu_pfb_passes_conformance() {
    let Some(ctx) = context_or_skip() else { return };
    for in_flight in [0usize, 2] {
        pfb_suite(&format!("gpu-wgpu (in-flight {in_flight})"), &|c| {
            let ctx = ctx.clone();
            BatchPfb::new(c.clone(), move |g| {
                Ok(Box::new(WgpuPfbExecutor::new(ctx, g, in_flight)))
            })
            .map(|p| Box::new(p) as Box<dyn PfbBackend>)
            .map_err(|e| e.to_string())
        })
        .assert_passed();
    }
}
