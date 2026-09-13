//! Compute providers and runtime selection (T-041, ADR-0007).
//!
//! The DSP seams — [`FftBackend`], [`SpectralBackend`] (STFT rows) and [`PfbBackend`] — have one
//! implementation per provider:
//!
//! | Provider | [`ProviderKind`] | Build | FFT | STFT rows | PFB |
//! |---|---|---|---|---|---|
//! | CPU reference | `cpu` | always | rustfft | [`CpuSpectral`] | [`crate::Pfb`] |
//! | CPU multi-threaded | `cpu-mt` | `cpu-mt` (default) | — | [`CpuMtSpectral`] | `BatchPfb` + `MtExecutor` |
//! | Apple Accelerate | `accelerate` | `accelerate`, macOS | vDSP | `AccelerateSpectral` | — |
//! | GPU (wgpu: Metal / Vulkan) | `gpu` | `gpu-wgpu` | `WgpuFft` | `WgpuSpectral` | `BatchPfb` + `WgpuPfbExecutor` |
//! | CUDA (Jetson, T-026) | `cuda` | `gpu` | stub | — | stub |
//!
//! **Selection.** [`Compute`] picks a provider per workload from [`ComputeOptions`] (config or
//! the `HK_COMPUTE*` environment variables): `auto`, or a named provider. A provider is used
//! only if it is compiled in, **marked conformant** ([`ProviderKind::conformant`]: it passes
//! [`crate::conformance`]), usable at runtime (device present, GPU self-check passed) and
//! supports the size. Otherwise selection falls back — `gpu`/`accelerate` → `cpu-mt` → `cpu` —
//! and the returned [`Selection`] carries the reason, which is also logged to stderr.
//!
//! **Auto policy** (measured on the M3 Ultra, ADR-0007 table): STFT rows → `gpu` (asynchronous)
//! → `accelerate` → `cpu`; PFB → `gpu` → `cpu-mt` → `cpu`; a single FFT → `cpu`. A named
//! provider falls back through the same workload chain. The multi-threaded CPU is not in the
//! STFT chain: its per-segment accumulation is serial, so it does not beat one thread there.

#[cfg(feature = "cpu-mt")]
pub mod pool;
pub mod spectral;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use spectral::{CpuSpectral, SpectralBackend};

#[cfg(feature = "cpu-mt")]
pub use spectral::CpuMtSpectral;

use crate::channelizer::{ChannelizerError, Pfb, PfbBackend, PfbConfig};
use crate::fft::{CpuFft, FftBackend};
use crate::stft::{StftConfig, StftProcessor};
use crate::welch::ConfigError;
use crate::window::Window;

/// A requested provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preference {
    /// The measured default per workload (module docs).
    #[default]
    Auto,
    /// CPU reference (single thread).
    Cpu,
    /// CPU multi-threaded.
    CpuMt,
    /// Apple Accelerate (vDSP).
    Accelerate,
    /// GPU via wgpu (Metal on macOS, Vulkan on Linux/Jetson).
    Gpu,
    /// CUDA (Jetson; not implemented yet, T-026).
    Cuda,
}

impl FromStr for Preference {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(Self::Auto),
            "cpu" | "cpu-reference" => Ok(Self::Cpu),
            "cpu-mt" | "cpumt" | "mt" => Ok(Self::CpuMt),
            "accelerate" | "vdsp" => Ok(Self::Accelerate),
            "gpu" | "wgpu" | "metal" | "vulkan" => Ok(Self::Gpu),
            "cuda" => Ok(Self::Cuda),
            other => Err(format!(
                "unknown compute provider {other:?} (auto|cpu|cpu-mt|accelerate|gpu|cuda)"
            )),
        }
    }
}

impl fmt::Display for Preference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::CpuMt => "cpu-mt",
            Self::Accelerate => "accelerate",
            Self::Gpu => "gpu",
            Self::Cuda => "cuda",
        })
    }
}

/// A concrete provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// rustfft, one thread: the reference every other provider is checked against.
    CpuReference,
    /// rustfft on the shared rayon pool.
    CpuMt,
    /// Apple Accelerate vDSP.
    Accelerate,
    /// wgpu compute.
    GpuWgpu,
    /// CUDA on the Jetson (T-026).
    Cuda,
}

impl ProviderKind {
    /// Every provider, in fallback-preference order from the most specialised.
    pub const ALL: [ProviderKind; 5] = [
        ProviderKind::Cuda,
        ProviderKind::GpuWgpu,
        ProviderKind::Accelerate,
        ProviderKind::CpuMt,
        ProviderKind::CpuReference,
    ];

    /// Short name (matches [`Preference`] spelling).
    pub fn name(self) -> &'static str {
        match self {
            Self::CpuReference => "cpu",
            Self::CpuMt => "cpu-mt",
            Self::Accelerate => "accelerate",
            Self::GpuWgpu => "gpu",
            Self::Cuda => "cuda",
        }
    }

    /// Compiled into this build.
    pub fn compiled(self) -> bool {
        match self {
            Self::CpuReference => true,
            Self::CpuMt => cfg!(feature = "cpu-mt"),
            Self::Accelerate => cfg!(all(feature = "accelerate", target_os = "macos")),
            Self::GpuWgpu => cfg!(feature = "gpu-wgpu"),
            Self::Cuda => cfg!(feature = "gpu"),
        }
    }

    /// Passes the [`crate::conformance`] suite (its test binary exists and is green). Selection
    /// refuses providers that are not; a new provider (CUDA, T-026) flips this only once its
    /// conformance binary passes on the target.
    pub fn conformant(self) -> bool {
        match self {
            Self::CpuReference | Self::CpuMt | Self::Accelerate | Self::GpuWgpu => true,
            Self::Cuda => false,
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Runtime compute settings; a serde data spec
/// (`{"provider": "auto", "pfb": "gpu", "threads": 8, "gpu_in_flight": 2}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComputeOptions {
    /// Provider for every workload unless overridden below.
    pub provider: Preference,
    /// Override for STFT rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stft: Option<Preference>,
    /// Override for the PFB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pfb: Option<Preference>,
    /// Worker threads of the shared CPU pool (first use wins; default: all cores).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<usize>,
    /// Batches an asynchronous (GPU) provider may run ahead of delivery; 0 = synchronous.
    pub gpu_in_flight: usize,
}

impl Default for ComputeOptions {
    fn default() -> Self {
        Self {
            provider: Preference::Auto,
            stft: None,
            pfb: None,
            threads: None,
            gpu_in_flight: 2,
        }
    }
}

impl ComputeOptions {
    /// Defaults overridden by the environment ([`ComputeOptions::with_env`]).
    pub fn from_env() -> Result<Self, String> {
        Self::default().with_env()
    }

    /// Applies `HK_COMPUTE` (provider), `HK_COMPUTE_STFT`, `HK_COMPUTE_PFB`,
    /// `HK_COMPUTE_THREADS` and `HK_GPU_IN_FLIGHT` over these options (environment wins, so an
    /// operator can force a provider without editing the scan plan).
    pub fn with_env(mut self) -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        if let Some(v) = var("HK_COMPUTE") {
            self.provider = v.parse()?;
        }
        if let Some(v) = var("HK_COMPUTE_STFT") {
            self.stft = Some(v.parse()?);
        }
        if let Some(v) = var("HK_COMPUTE_PFB") {
            self.pfb = Some(v.parse()?);
        }
        if let Some(v) = var("HK_COMPUTE_THREADS") {
            self.threads = Some(
                v.trim()
                    .parse()
                    .map_err(|e| format!("HK_COMPUTE_THREADS={v:?}: {e}"))?,
            );
        }
        if let Some(v) = var("HK_GPU_IN_FLIGHT") {
            self.gpu_in_flight = v
                .trim()
                .parse()
                .map_err(|e| format!("HK_GPU_IN_FLIGHT={v:?}: {e}"))?;
        }
        Ok(self)
    }
}

/// What a provider is asked to compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Workload {
    /// A plain forward FFT.
    Fft,
    /// STFT segment rows.
    Stft,
    /// Polyphase filter bank.
    Pfb,
}

/// The outcome of one selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Workload.
    pub workload: Workload,
    /// What was asked for.
    pub requested: Preference,
    /// What is used.
    pub provider: ProviderKind,
    /// Why a requested provider was not used (each refused candidate, in order), if any.
    pub fallback: Option<String>,
}

impl fmt::Display for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?}: requested {}, using {}",
            self.workload, self.requested, self.provider
        )?;
        if let Some(why) = &self.fallback {
            write!(f, " ({why})")?;
        }
        Ok(())
    }
}

/// Availability of one provider in this build and process.
#[derive(Clone, Debug)]
pub struct ProviderStatus {
    /// Provider.
    pub kind: ProviderKind,
    /// Compiled in.
    pub compiled: bool,
    /// Marked conformant.
    pub conformant: bool,
    /// `Ok(detail)` if usable now, else why not.
    pub usable: Result<String, String>,
}

/// Provider registry and factory. Cheap to clone; share one per process.
#[derive(Clone)]
pub struct Compute {
    options: ComputeOptions,
    #[cfg(feature = "gpu-wgpu")]
    gpu: Arc<std::sync::OnceLock<Result<Arc<crate::gpu_wgpu::GpuContext>, String>>>,
    #[cfg(not(feature = "gpu-wgpu"))]
    _gpu: Arc<()>,
}

type Attempt<T> = Result<T, String>;

impl Compute {
    /// A registry with `options`.
    pub fn new(options: ComputeOptions) -> Self {
        Self {
            options,
            #[cfg(feature = "gpu-wgpu")]
            gpu: Arc::new(std::sync::OnceLock::new()),
            #[cfg(not(feature = "gpu-wgpu"))]
            _gpu: Arc::new(()),
        }
    }

    /// Defaults plus `HK_COMPUTE*` environment overrides.
    pub fn from_env() -> Result<Self, String> {
        Ok(Self::new(ComputeOptions::from_env()?))
    }

    /// The options.
    pub fn options(&self) -> &ComputeOptions {
        &self.options
    }

    /// Every provider's status (probes the GPU once).
    pub fn status(&self) -> Vec<ProviderStatus> {
        ProviderKind::ALL
            .iter()
            .map(|&kind| ProviderStatus {
                kind,
                compiled: kind.compiled(),
                conformant: kind.conformant(),
                usable: self.usable(kind),
            })
            .collect()
    }

    fn usable(&self, kind: ProviderKind) -> Result<String, String> {
        if !kind.compiled() {
            return Err(format!("{kind} is not compiled into this build"));
        }
        if !kind.conformant() {
            return Err(format!(
                "{kind} is not marked conformant (ADR-0007 conformance suite)"
            ));
        }
        match kind {
            ProviderKind::CpuReference => Ok("rustfft, 1 thread".into()),
            ProviderKind::CpuMt => self.pool_description(),
            ProviderKind::Accelerate => Ok("vDSP DFT".into()),
            ProviderKind::GpuWgpu => self.gpu_description(),
            ProviderKind::Cuda => Err("CUDA kernels are not linked yet (T-026)".into()),
        }
    }

    #[cfg(feature = "cpu-mt")]
    fn pool(&self) -> Result<Arc<rayon::ThreadPool>, String> {
        pool::shared(self.options.threads)
    }

    #[cfg(feature = "cpu-mt")]
    fn pool_description(&self) -> Result<String, String> {
        self.pool()
            .map(|p| format!("rustfft on {} pool threads", p.current_num_threads()))
    }

    #[cfg(not(feature = "cpu-mt"))]
    fn pool_description(&self) -> Result<String, String> {
        Err("cpu-mt is not compiled into this build".into())
    }

    #[cfg(feature = "gpu-wgpu")]
    fn gpu_description(&self) -> Result<String, String> {
        self.gpu_context().map(|c| c.describe())
    }

    #[cfg(not(feature = "gpu-wgpu"))]
    fn gpu_description(&self) -> Result<String, String> {
        Err("gpu-wgpu is not compiled into this build".into())
    }

    /// The GPU context, created and self-checked on first use.
    #[cfg(feature = "gpu-wgpu")]
    pub fn gpu_context(&self) -> Result<Arc<crate::gpu_wgpu::GpuContext>, String> {
        self.gpu
            .get_or_init(|| {
                let ctx = crate::gpu_wgpu::context()?;
                gpu_self_check(&ctx)?;
                Ok(ctx)
            })
            .clone()
    }

    fn preference(&self, workload: Workload) -> Preference {
        let specific = match workload {
            Workload::Stft => self.options.stft,
            Workload::Pfb => self.options.pfb,
            Workload::Fft => None,
        };
        specific.unwrap_or(self.options.provider)
    }

    /// Candidate order for a request.
    fn candidates(requested: Preference, workload: Workload, n: usize) -> Vec<ProviderKind> {
        use ProviderKind as K;
        let _ = n;
        // What each workload falls back through after the requested provider (measured: the
        // multi-threaded CPU helps the PFB, not STFT rows, whose accumulation is serial).
        let fallback: &[ProviderKind] = match workload {
            Workload::Pfb => &[K::CpuMt, K::CpuReference],
            Workload::Stft => &[K::Accelerate, K::CpuReference],
            Workload::Fft => &[K::Accelerate, K::CpuReference],
        };
        let mut order = match requested {
            Preference::Auto => match workload {
                Workload::Pfb | Workload::Stft => vec![K::GpuWgpu],
                Workload::Fft => vec![K::CpuReference],
            },
            Preference::Cpu => vec![K::CpuReference],
            Preference::CpuMt => vec![K::CpuMt],
            Preference::Accelerate => vec![K::Accelerate],
            Preference::Gpu => vec![K::GpuWgpu],
            Preference::Cuda => vec![K::Cuda],
        };
        if requested != Preference::Cpu {
            for &k in fallback {
                if !order.contains(&k) {
                    order.push(k);
                }
            }
        }
        order
    }

    fn select<T>(
        &self,
        workload: Workload,
        n: usize,
        mut build: impl FnMut(ProviderKind) -> Attempt<T>,
    ) -> (T, Selection) {
        let requested = self.preference(workload);
        let mut reasons = Vec::new();
        for kind in Self::candidates(requested, workload, n) {
            let attempt = self.usable(kind).and_then(|_| build(kind));
            match attempt {
                Ok(value) => {
                    let fallback = (!reasons.is_empty()).then(|| reasons.join("; "));
                    let selection = Selection {
                        workload,
                        requested,
                        provider: kind,
                        fallback,
                    };
                    if selection.fallback.is_some() && requested != Preference::Auto {
                        eprintln!("hk-dsp compute: {selection}");
                    }
                    return (value, selection);
                }
                Err(why) => reasons.push(format!("{kind}: {why}")),
            }
        }
        unreachable!("the CPU reference always builds: {reasons:?}")
    }

    /// A forward FFT of length `len`.
    pub fn fft(&self, len: usize) -> (Box<dyn FftBackend>, Selection) {
        self.select(Workload::Fft, len, |kind| -> Attempt<Box<dyn FftBackend>> {
            match kind {
                ProviderKind::CpuReference => Ok(Box::new(CpuFft::new(len))),
                #[cfg(all(feature = "accelerate", target_os = "macos"))]
                ProviderKind::Accelerate => crate::accelerate::AccelerateFft::new(len)
                    .map(|f| Box::new(f) as Box<dyn FftBackend>),
                #[cfg(feature = "gpu-wgpu")]
                ProviderKind::GpuWgpu => crate::gpu_wgpu::WgpuFft::new(self.gpu_context()?, len)
                    .map(|f| Box::new(f) as Box<dyn FftBackend>),
                ProviderKind::CpuMt => Err("no multi-threaded single-FFT provider".into()),
                other => Err(format!("{other} provides no FFT in this build")),
            }
        })
    }

    /// STFT rows for `config` (window, FFT length and hop).
    pub fn spectral(&self, config: &StftConfig) -> (Box<dyn SpectralBackend>, Selection) {
        let n = config.welch.fft_len;
        let window = Window::new(config.welch.window, n);
        self.select(
            Workload::Stft,
            n,
            |kind| -> Attempt<Box<dyn SpectralBackend>> {
                match kind {
                    ProviderKind::CpuReference => Ok(Box::new(CpuSpectral::new(&window))),
                    #[cfg(feature = "cpu-mt")]
                    ProviderKind::CpuMt => Ok(Box::new(CpuMtSpectral::new(&window, self.pool()?))),
                    #[cfg(all(feature = "accelerate", target_os = "macos"))]
                    ProviderKind::Accelerate => crate::accelerate::AccelerateSpectral::new(&window)
                        .map(|s| Box::new(s) as Box<dyn SpectralBackend>),
                    #[cfg(feature = "gpu-wgpu")]
                    ProviderKind::GpuWgpu => Ok(Box::new(crate::gpu_wgpu::WgpuSpectral::new(
                        self.gpu_context()?,
                        &window,
                        config.welch.hop(),
                        self.options.gpu_in_flight,
                    ))),
                    other => Err(format!("{other} provides no STFT rows in this build")),
                }
            },
        )
    }

    /// An STFT processor for `config` on the selected provider. With an asynchronous
    /// provider, call [`StftProcessor::flush`] at the end of a stream.
    pub fn stft(&self, config: StftConfig) -> Result<(StftProcessor, Selection), ConfigError> {
        config.validate()?;
        let (backend, selection) = self.spectral(&config);
        Ok((StftProcessor::with_spectral(config, backend)?, selection))
    }

    /// A PFB for `config` on the selected provider. With an asynchronous provider, drain
    /// [`PfbBackend::flush`] at the end of a stream.
    pub fn pfb(
        &self,
        config: PfbConfig,
    ) -> Result<(Box<dyn PfbBackend>, Selection), ChannelizerError> {
        config.validate()?;
        // A prototype that cannot be designed fails every provider alike: report it up front.
        crate::filter::pfb_prototype(config.channels, config.stopband_db)?;
        Ok(self.select(
            Workload::Pfb,
            config.channels,
            |kind| -> Attempt<Box<dyn PfbBackend>> {
                let built: Result<Box<dyn PfbBackend>, ChannelizerError> = match kind {
                    ProviderKind::CpuReference => {
                        Pfb::new(config.clone()).map(|p| Box::new(p) as Box<dyn PfbBackend>)
                    }
                    #[cfg(feature = "cpu-mt")]
                    ProviderKind::CpuMt => {
                        let pool = self.pool()?;
                        crate::channelizer::batch::BatchPfb::new(config.clone(), move |g| {
                            Ok(Box::new(crate::channelizer::batch::MtExecutor::new(
                                g, pool,
                            )))
                        })
                        .map(|p| Box::new(p) as Box<dyn PfbBackend>)
                    }
                    #[cfg(feature = "gpu-wgpu")]
                    ProviderKind::GpuWgpu => {
                        let ctx = self.gpu_context()?;
                        let in_flight = self.options.gpu_in_flight;
                        crate::channelizer::batch::BatchPfb::new(config.clone(), move |g| {
                            Ok(Box::new(crate::gpu_wgpu::WgpuPfbExecutor::new(
                                ctx, g, in_flight,
                            )))
                        })
                        .map(|p| Box::new(p) as Box<dyn PfbBackend>)
                    }
                    other => return Err(format!("{other} provides no PFB in this build")),
                };
                built.map_err(|e| e.to_string())
            },
        ))
    }
}

/// A quick numeric check of the GPU against rustfft before first use (power-of-two and
/// Bluestein paths); refuses the GPU if a driver produces wrong answers.
#[cfg(feature = "gpu-wgpu")]
fn gpu_self_check(ctx: &Arc<crate::gpu_wgpu::GpuContext>) -> Result<(), String> {
    use crate::synth::{Rng, complex_noise};
    for n in [64usize, 100] {
        let x = complex_noise(&mut Rng::new(n as u64), n, 1.0);
        let mut want = x.clone();
        CpuFft::new(n).forward(&mut want);
        let mut got = x;
        crate::gpu_wgpu::WgpuFft::new(ctx.clone(), n)?.forward(&mut got);
        let err = crate::conformance::complex_rel(&got, &want);
        if err > crate::conformance::TOLERANCES.complex_rel {
            return Err(format!(
                "GPU self-check failed on {} (N={n}: error {err:.2e})",
                ctx.describe()
            ));
        }
    }
    Ok(())
}
