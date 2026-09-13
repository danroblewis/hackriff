//! Single-core throughput of the streaming Welch PSD + spectral kurtosis (T-004).
//!
//! Run with `cargo bench -p hk-dsp --bench spectral_throughput`. Prints Msps (input samples per
//! second, one thread) for 4096-bin Hann frames, K = 16, from `Complex32` and `Complex<i8>`.

use std::hint::black_box;
use std::time::{Duration, Instant};

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{InputInfo, IqSample, StftConfig, StftProcessor, WelchConfig};
use hk_model::{Provenance, SampleTime, Timestamp};

const FS: f64 = 20e6;
const BLOCK: usize = 65_536;

fn provenance() -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:bench",
        "tune": {"center_hz": 100e6, "sample_rate_hz": FS, "lna_db": 16.0, "vga_db": 20.0,
                 "amp_on": false, "bandwidth_hz": 15e6},
        "clip_count": 0, "overload": false, "clock_source": "internal", "clock_locked": true,
        "timestamp_method": "synthetic",
    });
    ProvenanceHandle::new(serde_json::from_value::<Provenance>(json).unwrap())
}

fn run<T: IqSample>(config: StftConfig, data: &[T], prov: &ProvenanceHandle) -> f64 {
    let mut stft = StftProcessor::new(config).unwrap();
    let mut index = 0u64;
    let mut frames = 0u64;
    let feed = |stft: &mut StftProcessor, index: &mut u64, frames: &mut u64| {
        for block in data.chunks_exact(BLOCK) {
            let h = BlockHeader {
                time: SampleTime {
                    sample_index: *index,
                    host_time: Timestamp::UNIX_EPOCH,
                },
                provenance: prov.clone(),
                discontinuity: Discontinuity::NONE,
                dropped_before: 0,
            };
            stft.push(InputInfo::from(&h), block, |f| {
                black_box(f.spectrum.sk[0]);
                *frames += 1;
            });
            *index += BLOCK as u64;
        }
    };
    feed(&mut stft, &mut index, &mut frames); // warm-up
    let start = Instant::now();
    let first = index;
    while start.elapsed() < Duration::from_secs(3) {
        feed(&mut stft, &mut index, &mut frames);
    }
    (index - first) as f64 / start.elapsed().as_secs_f64() / 1e6
}

fn main() {
    let prov = provenance();
    let mut rng = Rng::new(99);
    let f32_data = synth::complex_noise(&mut rng, 64 * BLOCK, 1e-2);
    let (i8_data, _) = synth::quantize_ci8(&f32_data);

    println!("hk-dsp spectral throughput, one thread, 4096-bin Hann, K = 16, 20 Msps provenance");
    println!("{:<34} {:>12} {:>12}", "config", "Complex32", "Complex<i8>");
    for (label, overlap, holds) in [
        ("50% overlap, SK", 2048, false),
        ("50% overlap, SK + max/min hold", 2048, true),
        ("0% overlap, SK", 0, false),
    ] {
        let config = StftConfig::new(
            WelchConfig {
                fft_len: 4096,
                overlap,
                holds,
                ..WelchConfig::new(4096)
            },
            16,
        );
        let a = run(config, &f32_data, &prov);
        let b = run(config, &i8_data, &prov);
        println!("{label:<34} {a:>9.1} Msps {b:>7.1} Msps");
    }
}
