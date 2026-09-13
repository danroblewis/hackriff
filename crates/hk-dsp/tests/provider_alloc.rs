//! Steady-state allocations per block of the batched providers (T-041). The batched PFB planner
//! with the reference executor allocates nothing per block (asserted, like `channelizer_no_alloc`).
//! The multi-threaded CPU and GPU providers are measured and printed: rayon's job injection and
//! wgpu's command recording allocate internally, bounded per block; no buffers, bind groups or
//! host vectors of ours are created or grown in steady state. Own test binary: it installs a
//! counting global allocator.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use common::*;
use hk_core::Discontinuity;
use hk_dsp::channelizer::batch::BatchPfb;
use hk_dsp::synth::{self, Rng};
use hk_dsp::{InputInfo, PfbBackend, PfbConfig, StftConfig, StftProcessor, WelchConfig};

struct Counting;

// Counts every thread (rayon and wgpu work on other threads too).
static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn note() {
    if COUNTING.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards every call to the system allocator unchanged; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations per block over `blocks` steady-state blocks after 4 warm-up blocks.
fn per_block_pfb(p: &mut dyn PfbBackend, blocks: usize) -> f64 {
    let fs = 2e6;
    let prov = provenance(433.92e6, fs);
    let block = 8192;
    let (x, _) = synth::quantize_ci8(&synth::complex_noise(&mut Rng::new(5), block, 1e-2));
    let mut next = 0u64;
    let mut step = |p: &mut dyn PfbBackend, counting: bool| {
        let h = header(next, &prov, Discontinuity::NONE);
        COUNTING.store(counting, Ordering::Relaxed);
        let frames = p.process_ci8(InputInfo::from(&h), &x).frames;
        COUNTING.store(false, Ordering::Relaxed);
        next += block as u64;
        frames
    };
    for _ in 0..4 {
        step(p, false);
    }
    ALLOCATIONS.store(0, Ordering::Relaxed);
    for _ in 0..blocks {
        step(p, true);
    }
    ALLOCATIONS.load(Ordering::Relaxed) as f64 / blocks as f64
}

fn per_block_stft(p: &mut StftProcessor, blocks: usize) -> f64 {
    let fs = 2e6;
    let prov = provenance(144e6, fs);
    let block = 8192;
    let (x, _) = synth::quantize_ci8(&synth::complex_noise(&mut Rng::new(6), block, 1e-2));
    let mut next = 0u64;
    let mut step = |p: &mut StftProcessor, counting: bool| {
        let h = header(next, &prov, Discontinuity::NONE);
        COUNTING.store(counting, Ordering::Relaxed);
        p.push(InputInfo::from(&h), &x, |_| {});
        COUNTING.store(false, Ordering::Relaxed);
        next += block as u64;
    };
    for _ in 0..4 {
        step(p, false);
    }
    ALLOCATIONS.store(0, Ordering::Relaxed);
    for _ in 0..blocks {
        step(p, true);
    }
    ALLOCATIONS.load(Ordering::Relaxed) as f64 / blocks as f64
}

#[test]
fn provider_allocations_per_block() {
    // One test so the global counter is not shared between concurrently running tests.
    let configs = [
        PfbConfig::new(64),
        PfbConfig {
            active: Some(vec![1, 5, 40]),
            raster_offset_hz: 3125.0,
            ..PfbConfig::new(256)
        },
    ];
    for config in &configs {
        let mut p = BatchPfb::serial(config.clone()).unwrap();
        let per = per_block_pfb(&mut p, 40);
        assert_eq!(per, 0.0, "batched PFB planner allocated {per}/block");
    }

    #[cfg(feature = "cpu-mt")]
    {
        let pool = hk_dsp::compute::pool::shared(None).unwrap();
        let p2 = pool.clone();
        let mut p = BatchPfb::new(PfbConfig::new(64), move |g| {
            Ok(Box::new(hk_dsp::channelizer::batch::MtExecutor::new(g, p2)))
        })
        .unwrap();
        eprintln!(
            "cpu-mt PFB: {:.1} allocations/block (rayon internals)",
            per_block_pfb(&mut p, 40)
        );
        let config = StftConfig::new(WelchConfig::new(1024), 8);
        let w = hk_dsp::Window::new(config.welch.window, 1024);
        let mut s = StftProcessor::with_spectral(
            config,
            Box::new(hk_dsp::compute::CpuMtSpectral::new(&w, pool)),
        )
        .unwrap();
        eprintln!(
            "cpu-mt STFT: {:.1} allocations/block (rayon internals)",
            per_block_stft(&mut s, 40)
        );
    }

    #[cfg(feature = "gpu-wgpu")]
    if let Ok(ctx) = hk_dsp::gpu_wgpu::context() {
        let c = ctx.clone();
        let mut p = BatchPfb::new(PfbConfig::new(64), move |g| {
            Ok(Box::new(hk_dsp::gpu_wgpu::WgpuPfbExecutor::new(c, g, 2)))
        })
        .unwrap();
        eprintln!(
            "gpu-wgpu PFB: {:.1} allocations/block (wgpu command recording and map callbacks)",
            per_block_pfb(&mut p, 40)
        );
        let config = StftConfig::new(WelchConfig::new(1024), 8);
        let w = hk_dsp::Window::new(config.welch.window, 1024);
        let mut s = StftProcessor::with_spectral(
            config,
            Box::new(hk_dsp::gpu_wgpu::WgpuSpectral::new(ctx, &w, 512, 2)),
        )
        .unwrap();
        eprintln!(
            "gpu-wgpu STFT: {:.1} allocations/block (wgpu command recording and map callbacks)",
            per_block_stft(&mut s, 40)
        );
    }
}
