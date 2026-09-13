//! Per-frame cost of `Detector::process` at 4096 bins (T-006).
//!
//! Run with `cargo bench -p hk-detect --bench detect_throughput`. Frames are Gamma(10) noise
//! (20 Msps, 2.048 ms frames) with a known floor, so only the detector is timed: classification,
//! components, flags, integration and emission.
//! - `noise`: flat noise (few cells reach the OS count).
//! - `fm-band`: 40 carriers 10–40 bins wide at 10–30 dB plus 20 bursts (≈ 20 % occupancy; the
//!   OS count on many cells, long components, splits, integrated evaluations).

use std::hint::black_box;
use std::time::Instant;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_detect::{ClipCount, Detector, DetectorConfig, FloorReference};
use hk_dsp::floor::{FloorFrame, FloorMethod, GainKey, gamma};
use hk_dsp::synth::Rng;
use hk_dsp::window::{Window, WindowKind};
use hk_dsp::{Resolution, Spectrum, SpectrumFrame};
use hk_model::{Provenance, SampleTime, SurveyId, Timestamp};

const FS: f64 = 20e6;
const BINS: usize = 4096;
const K: u32 = 10;

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

fn frame(prov: &ProvenanceHandle, psd: Vec<f32>) -> SpectrumFrame {
    let metrics = Window::new(WindowKind::Hann, BINS).metrics();
    SpectrumFrame {
        seq: 0,
        t: SampleTime {
            sample_index: 0,
            host_time: Timestamp::UNIX_EPOCH,
        },
        sample_count: u64::from(K) * BINS as u64,
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
                n_avg: K,
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
}

fn floor(f: &SpectrumFrame) -> FloorFrame {
    let ones = vec![1.0f32; BINS];
    FloorFrame {
        seq: 0,
        t: f.t,
        provenance: f.provenance.clone(),
        gain: GainKey::of(f),
        segment: 0,
        frames_in_segment: 1,
        reset: false,
        method: FloorMethod::BlockFcme,
        n_avg_effective: f64::from(K),
        f_center_hz: 98e6,
        bin_width_hz: FS / BINS as f64,
        valid: true,
        floor: ones.clone(),
        wide_floor: ones.clone(),
        band_floor: 1.0,
        block_floor: Vec::new(),
        block_valid: Vec::new(),
        block_iterations: Vec::new(),
        valid_blocks: 1,
        unconverged_blocks: 0,
        slow_floor: ones,
        slow_band_floor: 1.0,
        block_slow: Vec::new(),
        slow_ready: true,
        occupancy: 0.0,
        percentile: None,
        uncertainty_db: 0.5,
        statistical_uncertainty_db: 0.0,
        quantisation_floor_dbfs_per_hz: None,
        quantisation_margin_db: 3.0,
        quantisation_limited: false,
        impulsive: false,
        gate_released: false,
        impulsive_excess_db: 0.0,
        active_episodes: 0,
    }
}

fn frames(dense: bool, count: usize) -> Vec<SpectrumFrame> {
    let prov = provenance();
    let mut rng = Rng::new(if dense { 2 } else { 1 });
    let mut carriers = Vec::new();
    if dense {
        for i in 0..40 {
            let w = 10 + (rng.unit() * 30.0) as usize;
            let c = 100 + i * 96;
            carriers.push((c, w, 10f32.powf((10.0 + 20.0 * rng.unit() as f32) / 10.0)));
        }
    }
    (0..count)
        .map(|i| {
            let mut profile = vec![1.0f32; BINS];
            for &(c, w, lvl) in &carriers {
                for p in &mut profile[c..(c + w).min(BINS)] {
                    *p += lvl;
                }
            }
            if dense {
                for k in 0..20 {
                    if (i + 7 * k) % 60 < 5 {
                        let c = 150 + k * 190;
                        for p in &mut profile[c..c + 20] {
                            *p += 100.0;
                        }
                    }
                }
            }
            let psd = profile
                .iter()
                .map(|&m| m * gamma::sample_unit_mean(&mut rng, K) as f32)
                .collect();
            frame(&prov, psd)
        })
        .collect()
}

fn bench(name: &str, reference: FloorReference, frames: &mut [SpectrumFrame]) {
    let mut cfg = DetectorConfig::new(SurveyId::new());
    cfg.floor_reference = reference;
    let mut det = Detector::new(cfg).unwrap();
    let mut ff = floor(&frames[0]);
    let samples = u64::from(K) * BINS as u64;
    let mut index = 0u64;
    let mut emitted = 0u64;
    let mut run = |frames: &mut [SpectrumFrame],
                   det: &mut Detector,
                   ff: &mut FloorFrame,
                   emitted: &mut u64| {
        for f in frames.iter_mut() {
            f.t = SampleTime {
                sample_index: index,
                host_time: Timestamp::from_unix_nanos((index as f64 * 1e9 / FS) as i64),
            };
            index += samples;
            ff.t = f.t;
            det.process(black_box(f), ff, ClipCount::NONE, &mut |e| {
                if let hk_detect::DetectorEvent::Detection(_) = e {
                    *emitted += 1;
                }
            });
        }
    };
    for _ in 0..2 {
        run(frames, &mut det, &mut ff, &mut emitted); // warm-up
    }
    emitted = 0;
    let reps = 5;
    let start = Instant::now();
    for _ in 0..reps {
        run(frames, &mut det, &mut ff, &mut emitted);
    }
    let n = (reps * frames.len()) as f64;
    let per = start.elapsed().as_secs_f64() / n;
    let s = det.stats();
    println!(
        "{name:<28} {:>8.1} µs/frame ({:.1} % of a 2.048 ms frame); {:.2} detections/frame, {} integrated evaluations",
        per * 1e6,
        per / (u64::from(K) * BINS as u64) as f64 * FS * 100.0,
        emitted as f64 / n,
        s.evaluations
    );
}

fn classify_only(name: &str, frames: &[SpectrumFrame]) {
    use hk_detect::{Branches, CfarEngine, CfarWindow, DetectionProfile, Thresholds};
    let window = CfarWindow::default();
    let th = Thresholds::new(f64::from(K), &window, &DetectionProfile::standard(), 3.0);
    let mut engine = CfarEngine::new(window, BINS);
    let floor = vec![1.0f32; BINS];
    let mut codes = vec![0u8; BINS];
    let reps = 5;
    let start = Instant::now();
    for _ in 0..reps {
        for f in frames {
            black_box(engine.classify(
                &f.spectrum.psd,
                &floor,
                &th,
                Branches::Or,
                None,
                &mut codes,
            ));
        }
    }
    let per = start.elapsed().as_secs_f64() / (reps * frames.len()) as f64;
    println!(
        "{name:<28} {:>8.1} µs/frame (classification only)",
        per * 1e6
    );
}

fn main() {
    let mut noise = frames(false, 500);
    let mut dense = frames(true, 500);
    println!("hk-detect Detector::process, {BINS} bins, Gamma({K}) frames with a known floor");
    classify_only("noise", &noise);
    classify_only("fm-band", &dense);
    bench(
        "noise, per-frame reference",
        FloorReference::PerFrame,
        &mut noise,
    );
    bench(
        "fm-band, per-frame reference",
        FloorReference::PerFrame,
        &mut dense,
    );
    bench("fm-band, wide reference", FloorReference::Wide, &mut dense);
}
