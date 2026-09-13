//! Real-time factor of every compute provider at 20 Msps (T-041, ADR-0007 table).
//!
//! Run with `cargo bench -p hk-dsp --bench compute_providers` (add
//! `--features gpu-wgpu,accelerate` on the Mac). Input is 8-bit IQ in 65 536-sample blocks, as a
//! ring reader delivers it. For each workload and provider the bench repeats a timed run
//! (`HK_BENCH_REPEATS`, default 5; `HK_BENCH_SECONDS` per run, default 1.5) and prints the
//! minimum and median input Msps, the real-time factor (`Msps / 20`) and the median wall time
//! per block. The 1-minute load average is printed before every row: other builds on the
//! machine make CPU numbers noisy, so compare rows taken at similar load.
//!
//! Workloads: STFT at N = 1024 / 4096 / 16384 (Hann, 50 % overlap, K = 16, SK and holds), and
//! the PFB at M = 800 (the T-008 25 kHz raster at 20 Msps), 1600 (12.5 kHz) and 512 (power of
//! two), all channels materialised.

use std::hint::black_box;
use std::time::{Duration, Instant};

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_dsp::channelizer::batch::BatchPfb;
use hk_dsp::compute::{CpuSpectral, SpectralBackend};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{
    InputInfo, Pfb, PfbBackend, PfbConfig, StftConfig, StftProcessor, WelchConfig, Window,
};
use hk_model::{SampleTime, Timestamp};
use num_complex::Complex;

const FS: f64 = 20e6;
const BLOCK: usize = 65_536;

fn provenance() -> ProvenanceHandle {
    hk_dsp::conformance::provenance(100e6, FS, 16.0)
}

fn header(index: u64, prov: &ProvenanceHandle) -> BlockHeader {
    BlockHeader {
        time: SampleTime {
            sample_index: index,
            host_time: Timestamp::UNIX_EPOCH,
        },
        provenance: prov.clone(),
        discontinuity: Discontinuity::NONE,
        dropped_before: 0,
    }
}

fn load_average() -> f64 {
    let mut l = [0.0f64; 3];
    // SAFETY: getloadavg writes at most 3 doubles into the 3-element array.
    let n = unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    if n >= 1 { l[0] } else { f64::NAN }
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// One timed run: `(samples, elapsed, blocks)`.
type Run<'a> = Box<dyn FnMut(Duration) -> (u64, Duration, u64) + 'a>;

fn report(label: &str, mut run: Run<'_>) {
    // `HK_BENCH_ONLY=substr[,substr]` runs only rows whose label contains one of them.
    if let Ok(only) = std::env::var("HK_BENCH_ONLY") {
        if !only.split(',').any(|s| label.contains(s.trim())) {
            return;
        }
    }
    let repeats: usize = env_or("HK_BENCH_REPEATS", 5);
    let seconds: f64 = env_or("HK_BENCH_SECONDS", 1.5);
    let load = load_average();
    run(Duration::from_secs_f64(0.3)); // warm-up
    let mut msps = Vec::new();
    let mut ms_block = Vec::new();
    for _ in 0..repeats {
        let (samples, elapsed, blocks) = run(Duration::from_secs_f64(seconds));
        msps.push(samples as f64 / elapsed.as_secs_f64() / 1e6);
        ms_block.push(elapsed.as_secs_f64() * 1e3 / blocks as f64);
    }
    msps.sort_by(f64::total_cmp);
    ms_block.sort_by(f64::total_cmp);
    let median = msps[msps.len() / 2];
    println!(
        "{label:<36} load {load:>5.1}  min {:>7.1} Msps ({:>5.1}x)  median {:>7.1} Msps ({:>5.1}x)  {:>6.3} ms/block",
        msps[0],
        msps[0] / 20.0,
        median,
        median / 20.0,
        ms_block[ms_block.len() / 2],
    );
}

fn stft_run<'a>(
    mut p: StftProcessor,
    data: &'a [Complex<i8>],
    prov: &'a ProvenanceHandle,
) -> Run<'a> {
    let mut index = 0u64;
    Box::new(move |dur| {
        let start = Instant::now();
        let (mut samples, mut blocks) = (0u64, 0u64);
        while start.elapsed() < dur {
            for block in data.chunks_exact(BLOCK) {
                let h = header(index, prov);
                p.push(InputInfo::from(&h), block, |f| {
                    black_box(f.spectrum.psd[0]);
                });
                index += BLOCK as u64;
                samples += BLOCK as u64;
                blocks += 1;
            }
        }
        p.flush(|f| {
            black_box(f.spectrum.psd[0]);
        });
        (samples, start.elapsed(), blocks)
    })
}

fn pfb_run<'a>(
    mut p: Box<dyn PfbBackend>,
    data: &'a [Complex<i8>],
    prov: &'a ProvenanceHandle,
) -> Run<'a> {
    let mut index = 0u64;
    Box::new(move |dur| {
        let start = Instant::now();
        let (mut samples, mut blocks) = (0u64, 0u64);
        while start.elapsed() < dur {
            for block in data.chunks_exact(BLOCK) {
                let h = header(index, prov);
                let out = p.process_ci8(InputInfo::from(&h), block);
                black_box(out.samples().first());
                index += BLOCK as u64;
                samples += BLOCK as u64;
                blocks += 1;
            }
        }
        while let Some(out) = p.flush() {
            black_box(out.samples().first());
        }
        (samples, start.elapsed(), blocks)
    })
}

fn main() {
    let prov = provenance();
    let f32_data = synth::complex_noise(&mut Rng::new(7), 48 * BLOCK, 1e-2);
    let (data, _) = synth::quantize_ci8(&f32_data);
    println!(
        "hk-dsp compute providers at 20 Msps, {BLOCK}-sample ci8 blocks; {} hardware threads",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );

    #[cfg(feature = "gpu-wgpu")]
    let gpu = match hk_dsp::gpu_wgpu::context() {
        Ok(ctx) => {
            println!("GPU: {}", ctx.describe());
            Some(ctx)
        }
        Err(e) => {
            println!("GPU: unavailable ({e})");
            None
        }
    };
    #[cfg(feature = "cpu-mt")]
    let pool = hk_dsp::compute::pool::shared(None).expect("rayon pool");

    println!("\nSTFT (Hann, 50 % overlap, K = 16, SK + holds)");
    for n in [1024usize, 4096, 16384] {
        let config = StftConfig::new(WelchConfig::new(n), 16);
        let window = Window::new(config.welch.window, n);
        let with = |b: Box<dyn SpectralBackend>| StftProcessor::with_spectral(config, b).unwrap();
        report(
            &format!("N={n:<5} cpu"),
            stft_run(with(Box::new(CpuSpectral::new(&window))), &data, &prov),
        );
        #[cfg(feature = "cpu-mt")]
        report(
            &format!("N={n:<5} cpu-mt"),
            stft_run(
                with(Box::new(hk_dsp::compute::CpuMtSpectral::new(
                    &window,
                    pool.clone(),
                ))),
                &data,
                &prov,
            ),
        );
        #[cfg(all(feature = "accelerate", target_os = "macos"))]
        report(
            &format!("N={n:<5} accelerate"),
            stft_run(
                with(Box::new(
                    hk_dsp::accelerate::AccelerateSpectral::new(&window).unwrap(),
                )),
                &data,
                &prov,
            ),
        );
        #[cfg(feature = "gpu-wgpu")]
        if let Some(ctx) = &gpu {
            for in_flight in [0usize, 2] {
                report(
                    &format!("N={n:<5} gpu (in-flight {in_flight})"),
                    stft_run(
                        with(Box::new(hk_dsp::gpu_wgpu::WgpuSpectral::new(
                            ctx.clone(),
                            &window,
                            config.welch.hop(),
                            in_flight,
                        ))),
                        &data,
                        &prov,
                    ),
                );
            }
        }
    }

    println!("\nPFB (all channels materialised, 60 dB prototype)");
    for m in [800usize, 1600, 512] {
        let config = PfbConfig::new(m);
        let taps = Pfb::new(config.clone()).unwrap().taps();
        let label = |p: &str| format!("M={m:<4} L={taps:<5} {p}");
        report(
            &label("cpu"),
            pfb_run(Box::new(Pfb::new(config.clone()).unwrap()), &data, &prov),
        );
        #[cfg(feature = "cpu-mt")]
        {
            let pool = pool.clone();
            report(
                &label("cpu-mt"),
                pfb_run(
                    Box::new(
                        BatchPfb::new(config.clone(), move |g| {
                            Ok(Box::new(hk_dsp::channelizer::batch::MtExecutor::new(
                                g, pool,
                            )))
                        })
                        .unwrap(),
                    ),
                    &data,
                    &prov,
                ),
            );
        }
        #[cfg(feature = "gpu-wgpu")]
        if let Some(ctx) = &gpu {
            for in_flight in [0usize, 2] {
                let ctx = ctx.clone();
                report(
                    &label(&format!("gpu (in-flight {in_flight})")),
                    pfb_run(
                        Box::new(
                            BatchPfb::new(config.clone(), move |g| {
                                Ok(Box::new(hk_dsp::gpu_wgpu::WgpuPfbExecutor::new(
                                    ctx, g, in_flight,
                                )))
                            })
                            .unwrap(),
                        ),
                        &data,
                        &prov,
                    ),
                );
            }
        }
        let _ = BatchPfb::serial; // the serial batch executor is conformance-only
    }
}
