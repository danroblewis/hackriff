//! Single-core throughput of the PFB channelizer and the DDC (T-008).
//!
//! Run with `cargo bench -p hk-dsp --bench channelizer_throughput`. Prints input Msps (one
//! thread) and real-time headroom at a 20 Msps input, for `Complex32` and `Complex<i8>` input.

use std::hint::black_box;
use std::time::{Duration, Instant};

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{Ddc, DdcSpec, InputInfo, IqSample, Pfb, PfbConfig, ResampleKind};
use hk_model::{Provenance, SampleTime, Timestamp};

const FS: f64 = 20e6;
const BLOCK: usize = 65_536;
const RUN: Duration = Duration::from_secs(2);

fn provenance() -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:bench",
        "tune": {"center_hz": 100e6, "sample_rate_hz": FS, "lna_db": 16.0, "vga_db": 20.0,
                 "amp_on": false, "bandwidth_hz": 15e6},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic",
    });
    ProvenanceHandle::new(serde_json::from_value::<Provenance>(json).unwrap())
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

/// Feeds `data` in blocks until `RUN` elapses; returns input Msps.
fn measure<T: IqSample>(
    data: &[T],
    prov: &ProvenanceHandle,
    mut step: impl FnMut(InputInfo<'_>, &[T]),
) -> f64 {
    let mut index = 0u64;
    for block in data.chunks_exact(BLOCK) {
        let h = header(index, prov);
        step(InputInfo::from(&h), block);
        index += BLOCK as u64;
    }
    let first = index;
    let start = Instant::now();
    while start.elapsed() < RUN {
        for block in data.chunks_exact(BLOCK) {
            let h = header(index, prov);
            step(InputInfo::from(&h), block);
            index += BLOCK as u64;
        }
    }
    (index - first) as f64 / start.elapsed().as_secs_f64() / 1e6
}

fn pfb_rate<T: IqSample>(m: usize, data: &[T], prov: &ProvenanceHandle) -> f64 {
    let mut pfb = Pfb::new(PfbConfig::new(m)).unwrap();
    measure(data, prov, |info, block| {
        let out = pfb.process(info, block);
        black_box(out.samples().first());
    })
}

fn ddc_rate<T: IqSample>(spec: &DdcSpec, data: &[T], prov: &ProvenanceHandle) -> f64 {
    let mut ddc = Ddc::new(spec.clone(), FS).unwrap();
    measure(data, prov, |info, block| {
        let out = ddc.process(info, block).unwrap();
        black_box(out.samples.first());
    })
}

fn describe(spec: &DdcSpec) -> String {
    let ddc = Ddc::new(spec.clone(), FS).unwrap();
    let p = ddc.plan();
    let stage2 = match p.resample.as_ref() {
        None => "none".to_string(),
        Some(r) => {
            let kind = match r.kind {
                ResampleKind::Integer { decimation } => format!("/{decimation}"),
                ResampleKind::Rational { up, down } => format!("x{up}/{down}"),
                ResampleKind::Fractional { ratio, phases } => {
                    format!("frac {ratio:.4} ({phases}ph)")
                }
            };
            format!("{kind}, {} taps/phase", r.taps_per_phase)
        }
    };
    format!(
        "stage 1 /{} {} taps; stage 2 {stage2}",
        p.xlate_decimation,
        p.xlate.len()
    )
}

fn main() {
    let prov = provenance();
    let mut rng = Rng::new(7);
    let f32_data = synth::complex_noise(&mut rng, 32 * BLOCK, 1e-2);
    let (i8_data, _) = synth::quantize_ci8(&f32_data);

    println!("hk-dsp channelizer throughput, one thread, 20 Msps input, {BLOCK}-sample blocks");
    println!("{:<52} {:>18} {:>18}", "stage", "Complex32", "Complex<i8>");
    let row = |label: String, a: f64, b: f64| {
        println!(
            "{label:<52} {a:>8.1} Msps {:>4.1}x {b:>8.1} Msps {:>4.1}x",
            a / 20.0,
            b / 20.0
        );
    };
    for m in [64usize, 512] {
        let taps = Pfb::new(PfbConfig::new(m)).unwrap().taps();
        let a = pfb_rate(m, &f32_data, &prov);
        let b = pfb_rate(m, &i8_data, &prov);
        row(
            format!(
                "PFB M={m} ({taps} taps, {:.1} kS/s/ch)",
                2.0 * FS / m as f64 / 1e3
            ),
            a,
            b,
        );
    }
    for (label, spec) in [
        (
            "DDC 20 Msps -> 250 kS/s (WBFM, bw 200 kHz)",
            DdcSpec::new(2.5e6, 200e3).with_output_rate(250e3),
        ),
        (
            "DDC 20 Msps -> 48 kS/s (NBFM, bw 16 kHz)",
            DdcSpec::new(-3.1e6, 16e3).with_output_rate(48e3),
        ),
    ] {
        let a = ddc_rate(&spec, &f32_data, &prov);
        let b = ddc_rate(&spec, &i8_data, &prov);
        row(label.to_string(), a, b);
        println!("    plan: {}", describe(&spec));
    }
}
