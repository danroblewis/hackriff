//! GPU compute provider over wgpu (T-041, ADR-0007): Metal on macOS, Vulkan on Linux and the
//! Jetson. Compiled with the `gpu-wgpu` feature only.
//!
//! **Kernels** (`kernels.wgsl`): a batched radix-2 FFT (one dispatch per butterfly stage over
//! every item of a batch), Bluestein's algorithm for other lengths (two power-of-two transforms
//! with a precomputed chirp kernel), an STFT segment loader (window applied on the GPU), the PFB
//! polyphase fold, power rows (fftshifted) and a channel gather with per-frame raster
//! rotators. Tables (bit reversal, twiddles, chirp kernel, window/taps) are computed on the CPU
//! in `f64` and uploaded once per plan.
//!
//! **Batches and buffers.** An [`Engine`] owns a pool of *slots*; each slot has its own GPU
//! buffers and bind groups (created once, grown only when a larger batch arrives): a mappable
//! upload buffer, the storage buffers the kernels use, and a mappable readback buffer. A batch
//! is one command buffer: copy the upload into storage, run every pass, copy the output to the
//! readback buffer. Nothing is created per batch.
//!
//! **Asynchronous readback.** After `submit` the readback buffer is mapped asynchronously; the
//! slot stays in a FIFO until its map callback fires (checked with a non-blocking
//! `Device::poll`). Callers bound the FIFO (`max_in_flight`); only past that bound, or on flush,
//! does the caller block. So the ring reader stages and submits the next block while the GPU
//! works on earlier ones.
//!
//! **Numerics.** Single precision throughout; results match the CPU reference within the
//! conformance tolerances (not bitwise).

mod engine;
pub mod fft;
pub mod pfb;
pub mod spectral;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub use fft::WgpuFft;
pub use pfb::WgpuPfbExecutor;
pub use spectral::WgpuSpectral;

/// A shared wgpu device with the compiled kernels.
pub struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: wgpu::AdapterInfo,
    pipelines: Pipelines,
    failed: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
}

struct Pipelines {
    load_stft: wgpu::ComputePipeline,
    load_pfb: wgpu::ComputePipeline,
    fft_stage: wgpu::ComputePipeline,
    bluestein_mid: wgpu::ComputePipeline,
    post_power: wgpu::ComputePipeline,
    post_gather: wgpu::ComputePipeline,
}

/// How long a blocking wait for one batch may take before the device is declared stuck.
pub(crate) const WAIT_TIMEOUT: Duration = Duration::from_secs(30);

static CONTEXT: OnceLock<Result<Arc<GpuContext>, String>> = OnceLock::new();

/// The process-wide GPU context, created on first use. `Err` explains why no usable adapter
/// exists (no GPU, software rasteriser only, device request failed).
pub fn context() -> Result<Arc<GpuContext>, String> {
    CONTEXT.get_or_init(create).clone()
}

impl GpuContext {
    /// Adapter name and backend, e.g. `Apple M3 Ultra (Metal)`.
    pub fn describe(&self) -> String {
        format!("{} ({:?})", self.info.name, self.info.backend)
    }

    /// The adapter.
    pub fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.info
    }

    /// Panics if the device reported an uncaptured error (validation, out of memory, lost).
    pub(crate) fn check(&self) {
        if self.failed.load(Ordering::Relaxed) {
            let why = self
                .last_error
                .lock()
                .map(|e| e.clone().unwrap_or_default())
                .unwrap_or_default();
            panic!("hk-dsp gpu-wgpu: device error: {why}");
        }
    }
}

fn create() -> Result<Arc<GpuContext>, String> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
        ..Default::default()
    }))
    .map_err(|e| format!("no GPU adapter: {e}"))?;
    let info = adapter.get_info();
    if info.device_type == wgpu::DeviceType::Cpu {
        return Err(format!(
            "only a software adapter is available ({}, {:?})",
            info.name, info.backend
        ));
    }
    let supported = adapter.limits();
    let limits = wgpu::Limits {
        max_storage_buffer_binding_size: supported.max_storage_buffer_binding_size,
        max_buffer_size: supported.max_buffer_size,
        ..wgpu::Limits::default()
    };
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("hk-dsp compute"),
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .map_err(|e| format!("GPU device request failed on {}: {e}", info.name))?;

    let failed = Arc::new(AtomicBool::new(false));
    let last_error = Arc::new(Mutex::new(None));
    {
        let failed = failed.clone();
        let last_error = last_error.clone();
        device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
            eprintln!("hk-dsp gpu-wgpu: uncaptured device error: {e}");
            if let Ok(mut slot) = last_error.lock() {
                *slot = Some(e.to_string());
            }
            failed.store(true, Ordering::Relaxed);
        }));
    }

    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("hk-dsp kernels"),
        source: wgpu::ShaderSource::Wgsl(include_str!("kernels.wgsl").into()),
    });
    let pipeline = |entry: &str| {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: None,
            module: &module,
            entry_point: Some(entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        })
    };
    let pipelines = Pipelines {
        load_stft: pipeline("load_stft"),
        load_pfb: pipeline("load_pfb"),
        fft_stage: pipeline("fft_stage"),
        bluestein_mid: pipeline("bluestein_mid"),
        post_power: pipeline("post_power"),
        post_gather: pipeline("post_gather"),
    };
    let ctx = GpuContext {
        device,
        queue,
        info,
        pipelines,
        failed,
        last_error,
    };
    ctx.check();
    Ok(Arc::new(ctx))
}
