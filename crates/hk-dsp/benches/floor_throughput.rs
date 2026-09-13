//! Per-frame cost of the noise-floor tracker (T-005) at 4096 bins.
//!
//! Run with `cargo bench -p hk-dsp --bench floor_throughput`. Frames come from the real STFT
//! (20 Msps, Hann, K = 10, 50 % overlap) over noise with a few carriers; the tracker runs block
//! FCME + interpolation + occupancy + slow floor + gate, with and without the p20 cross-check.
//! Minimum statistics is timed separately.

use std::hint::black_box;
use std::time::{Duration, Instant};

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{FloorConfig, MinStatConfig, MinStatistics, NoiseFloorTracker};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{InputInfo, SpectrumFrame, StftConfig, StftProcessor, WelchConfig};
use hk_model::{Provenance, SampleTime, Timestamp};

const FS: f64 = 20e6;
const BINS: usize = 4096;

fn provenance() -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:bench",
        "tune": {"center_hz": 98e6, "sample_rate_hz": FS, "lna_db": 24.0, "vga_db": 20.0,
                 "amp_on": false, "bandwidth_hz": 15e6},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic",
    });
    ProvenanceHandle::new(serde_json::from_value::<Provenance>(json).unwrap())
}

fn frames() -> Vec<SpectrumFrame> {
    let prov = provenance();
    let mut stft = StftProcessor::new(StftConfig::new(WelchConfig::new(BINS), 10)).unwrap();
    let mut rng = Rng::new(1);
    let len = 64 * 10 * BINS / 2 + BINS;
    let mut iq = synth::complex_noise(&mut rng, len, 1e-3);
    for (i, off) in [-3.1e6, 0.45e6, 2.2e6, 6.0e6].into_iter().enumerate() {
        let t = synth::tone(0, len, off, FS, 1e-4, i as f64);
        synth::add_into(&mut iq, &t);
    }
    let h = BlockHeader {
        time: SampleTime {
            sample_index: 0,
            host_time: Timestamp::UNIX_EPOCH,
        },
        provenance: prov,
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
    };
    let mut out = Vec::new();
    stft.push(InputInfo::from(&h), &iq, |f| out.push(f.clone()));
    out
}

fn time_per_frame(mut f: impl FnMut(&SpectrumFrame), frames: &[SpectrumFrame]) -> f64 {
    for fr in frames {
        f(fr); // warm-up
    }
    let start = Instant::now();
    let mut n = 0u64;
    while start.elapsed() < Duration::from_secs(2) {
        for fr in &frames[1..] {
            f(fr);
            n += 1;
        }
    }
    start.elapsed().as_secs_f64() / n as f64 * 1e6
}

fn main() {
    let frames = frames();
    let period_ms = 10.0 * (BINS / 2) as f64 / FS * 1e3;
    println!(
        "noise-floor tracker, {BINS} bins, K = 10 (frame period {period_ms:.2} ms at 20 Msps)"
    );

    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let us = time_per_frame(
        |f| {
            black_box(tracker.update(f, |_| {}).band_floor);
        },
        &frames,
    );
    println!(
        "  tracker (FCME + p20 check): {us:8.1} µs/frame ({:.1} % of a frame period)",
        us / 10.0 / period_ms
    );

    let mut no_pct = NoiseFloorTracker::new(FloorConfig {
        percentile: None,
        ..FloorConfig::default()
    })
    .unwrap();
    let us = time_per_frame(
        |f| {
            black_box(no_pct.update(f, |_| {}).band_floor);
        },
        &frames,
    );
    println!("  tracker (FCME only):        {us:8.1} µs/frame");

    let mut minstat = MinStatistics::new(MinStatConfig::default(), BINS, 9.52).unwrap();
    let us = time_per_frame(
        |f| {
            minstat.update(&f.spectrum.psd);
            black_box(minstat.floor()[0]);
        },
        &frames,
    );
    println!("  minimum statistics:         {us:8.1} µs/frame");
}
