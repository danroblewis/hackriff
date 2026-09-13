//! Per-frame cost of the noise-floor tracker (T-005) at 4096 bins: mean, p99.9 and max.
//!
//! Run with `cargo bench -p hk-dsp --bench floor_throughput`. Workloads:
//! - **flat:** real STFT frames (20 Msps, Hann, K = 10, 50 % overlap) over noise with a few
//!   carriers; block FCME + shape + wide reference + occupancy + slow floor + gate, with and
//!   without the p20 cross-check.
//! - **shaped + episodes:** Gamma-domain frames of a HackRF-like baseband roll-off with a notch
//!   (the response shape is active, so shaped blocks run a second FCME) and a partial-band +6 dB
//!   floor rise switching every 1.5 s (rises, returns, ends).
//!
//! Minimum statistics is timed separately.

use std::hint::black_box;
use std::time::{Duration, Instant};

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_dsp::floor::{FloorConfig, MinStatConfig, MinStatistics, NoiseFloorTracker, gamma};
use hk_dsp::synth::{self, Rng};
use hk_dsp::window::{Window, WindowKind};
use hk_dsp::{
    InputInfo, Resolution, Spectrum, SpectrumFrame, StftConfig, StftProcessor, WelchConfig,
};
use hk_model::{Provenance, SampleTime, Timestamp};

const FS: f64 = 20e6;
const BINS: usize = 4096;
const UPDATES: usize = 20_000;

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

fn stft_frames() -> Vec<SpectrumFrame> {
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

/// 1464 Gamma-domain frames (3 s at 2.05 ms): roll-off + notch, +6 dB on bins 1200..2400 for
/// the second half.
fn shaped_frames() -> Vec<SpectrumFrame> {
    let prov = provenance();
    let metrics = Window::new(WindowKind::Hann, BINS).metrics();
    let base: Vec<f32> = (0..BINS)
        .map(|i| {
            let f = (i as f64 - BINS as f64 / 2.0).abs() / BINS as f64;
            let notch = if (2600..2900).contains(&i) { 0.01 } else { 1.0 };
            ((1.0 / (1.0 + (f / 0.375).powi(16)) + 10f64.powf(-2.5)) * notch) as f32
        })
        .collect();
    let mut rng = Rng::new(2);
    (0..1464u64)
        .map(|k| {
            let index = k * 10 * BINS as u64;
            let rise = k >= 732;
            let psd = base
                .iter()
                .enumerate()
                .map(|(i, &m)| {
                    let m = if rise && (1200..2400).contains(&i) {
                        m * 4.0
                    } else {
                        m
                    };
                    m * gamma::sample_unit_mean(&mut rng, 10) as f32
                })
                .collect();
            SpectrumFrame {
                seq: k,
                t: SampleTime {
                    sample_index: index,
                    host_time: Timestamp::from_unix_nanos((index as f64 * 1e9 / FS) as i64),
                },
                sample_count: 10 * BINS as u64,
                provenance: prov.clone(),
                provenance_changed: false,
                discontinuity: Discontinuity::NONE,
                dropped_samples: 0,
                spectrum: Spectrum {
                    f_center_hz: 98e6,
                    sample_rate_hz: FS,
                    resolution: Resolution {
                        window: WindowKind::Hann,
                        fft_len: BINS,
                        overlap: 0,
                        n_avg: 10,
                        bin_width_hz: FS / BINS as f64,
                        rbw_hz: metrics.enbw_bins * FS / BINS as f64,
                        window_metrics: metrics,
                    },
                    psd,
                    max_hold: Vec::new(),
                    min_hold: Vec::new(),
                    sk: Vec::new(),
                },
            }
        })
        .collect()
}

/// Per-update wall time over at least `UPDATES` calls after a warm-up pass: (mean, p99.9, max) µs.
fn time_per_frame(mut f: impl FnMut(&SpectrumFrame), frames: &[SpectrumFrame]) -> (f64, f64, f64) {
    for fr in frames {
        f(fr);
    }
    let mut times = Vec::with_capacity(UPDATES + frames.len());
    let start = Instant::now();
    while times.len() < UPDATES || start.elapsed() < Duration::from_secs(1) {
        for fr in &frames[1..] {
            let t0 = Instant::now();
            f(fr);
            times.push(t0.elapsed().as_secs_f64() * 1e6);
        }
    }
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    times.sort_by(f64::total_cmp);
    let p999 = times[(times.len() - 1) * 999 / 1000];
    (mean, p999, *times.last().unwrap())
}

fn report(name: &str, (mean, p999, max): (f64, f64, f64), period_ms: f64) {
    println!(
        "  {name:<34} mean {mean:7.1} µs  p99.9 {p999:7.1} µs  max {max:8.1} µs  (mean {:.1} % of a frame period)",
        mean / 10.0 / period_ms
    );
}

fn main() {
    let period_ms = 10.0 * (BINS / 2) as f64 / FS * 1e3;
    println!(
        "noise-floor tracker, {BINS} bins, K = 10 (frame period {period_ms:.2} ms at 20 Msps with 50 % overlap)"
    );
    let frames = stft_frames();

    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let t = time_per_frame(
        |f| {
            black_box(tracker.update(f, |_| {}).band_floor);
        },
        &frames,
    );
    report("flat, FCME + p20 check", t, period_ms);

    let mut no_pct = NoiseFloorTracker::new(FloorConfig {
        percentile: None,
        ..FloorConfig::default()
    })
    .unwrap();
    let t = time_per_frame(
        |f| {
            black_box(no_pct.update(f, |_| {}).band_floor);
        },
        &frames,
    );
    report("flat, FCME only", t, period_ms);
    let flat_shaped = no_pct
        .last()
        .unwrap()
        .shape
        .iter()
        .filter(|&&s| s < 1.0)
        .count();
    println!("    ({flat_shaped} shaped bins)");

    let mut no_shape = NoiseFloorTracker::new(FloorConfig {
        percentile: None,
        wide: hk_dsp::floor::WideReferenceConfig {
            learn_shape: false,
            ..Default::default()
        },
        ..FloorConfig::default()
    })
    .unwrap();
    let t = time_per_frame(
        |f| {
            black_box(no_shape.update(f, |_| {}).band_floor);
        },
        &frames,
    );
    report("flat, FCME only, shape off", t, period_ms);

    let shaped = shaped_frames();
    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut events = 0u64;
    let t = time_per_frame(
        |f| {
            black_box(tracker.update(f, |_| events += 1).band_floor);
        },
        &shaped,
    );
    let shaped_bins = tracker
        .last()
        .unwrap()
        .shape
        .iter()
        .filter(|&&s| s < 1.0)
        .count();
    report("shaped + episodes, FCME + p20", t, period_ms);
    println!("    ({shaped_bins} shaped bins, {events} events)");

    let mut minstat = MinStatistics::new(MinStatConfig::default(), BINS, 9.52).unwrap();
    let t = time_per_frame(
        |f| {
            minstat.update(&f.spectrum.psd);
            black_box(minstat.floor()[0]);
        },
        &frames,
    );
    report("minimum statistics", t, period_ms);
}
