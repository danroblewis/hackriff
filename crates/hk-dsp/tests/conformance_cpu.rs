//! Conformance suite (ADR-0007) for the CPU providers: the CPU reference itself (a sanity check
//! of the suite), the batched serial PFB, and the multi-threaded CPU provider. Runs in the
//! default `cargo test` / `just test`, no hardware.

use hk_dsp::channelizer::batch::BatchPfb;
use hk_dsp::compute::{CpuSpectral, SpectralBackend};
use hk_dsp::conformance::{fft_suite, pfb_suite, spectral_suite};
use hk_dsp::{CpuFft, FftBackend, Pfb, PfbBackend, Window};

#[test]
fn cpu_reference_passes_conformance() {
    fft_suite("cpu-rustfft", &|n| {
        Ok(Box::new(CpuFft::new(n)) as Box<dyn FftBackend>)
    })
    .assert_passed();
    spectral_suite("cpu-reference", &|c| {
        let w = Window::new(c.welch.window, c.welch.fft_len);
        Ok(Box::new(CpuSpectral::new(&w)) as Box<dyn SpectralBackend>)
    })
    .assert_passed();
    pfb_suite("cpu-pfb", &|c| {
        Pfb::new(c.clone())
            .map(|p| Box::new(p) as Box<dyn PfbBackend>)
            .map_err(|e| e.to_string())
    })
    .assert_passed();
}

#[test]
fn cpu_batch_pfb_passes_conformance() {
    pfb_suite("cpu-pfb-batch", &|c| {
        BatchPfb::serial(c.clone())
            .map(|p| Box::new(p) as Box<dyn PfbBackend>)
            .map_err(|e| e.to_string())
    })
    .assert_passed();
}

#[cfg(feature = "cpu-mt")]
#[test]
fn cpu_mt_passes_conformance() {
    use hk_dsp::channelizer::batch::MtExecutor;
    use hk_dsp::compute::CpuMtSpectral;
    let pool = hk_dsp::compute::pool::shared(None).unwrap();
    let p = pool.clone();
    spectral_suite("cpu-mt", &move |c| {
        let w = Window::new(c.welch.window, c.welch.fft_len);
        Ok(Box::new(CpuMtSpectral::new(&w, p.clone())) as Box<dyn SpectralBackend>)
    })
    .assert_passed();
    pfb_suite("cpu-mt", &move |c| {
        let pool = pool.clone();
        BatchPfb::new(c.clone(), move |g| Ok(Box::new(MtExecutor::new(g, pool))))
            .map(|p| Box::new(p) as Box<dyn PfbBackend>)
            .map_err(|e| e.to_string())
    })
    .assert_passed();
}
