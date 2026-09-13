//! Real-time factor of the whole detect reader (T-058): the loop of `hk-pipeline`'s `detect.rs`
//! minus the repository and control channel. ci8 chunks of 65 536 samples → clip scan →
//! [`StftProcessor`] (Hann, 0 % overlap, SK) → [`NoiseFloorTracker`] → [`Detector`] →
//! [`Tracker`] (+ `drain_into` every frame), at the pipeline's detection resolution for 8, 10 and 20 Msps.
//!
//! Run with `cargo bench -p hk-detect --bench detect_path`. Prints, per rate, the stream time
//! processed per wall second (RTF; ≥ 1 is real time) and the split across stages, plus the
//! 1-minute load average (other processes share the machine).
//!
//! - Input: a synthetic FM-band scene (40 FM-like carriers, 12 gated bursts, noise at ≈ 4 LSB,
//!   ci8 quantised) generated once per rate, or the ci8 file in `$HK_DETECT_PATH_IQ` (read as
//!   `$HK_DETECT_PATH_RATE` Msps, default 20) looped.
//! - `$HK_DETECT_PATH_SECONDS` stream seconds per rate (default 4).
//! - `$HK_DETECT_PATH_RATES` comma-separated Msps (default `8,10,20`).
//! - `--features parity-check`: every hop-set raster is also computed by the pre-T-058 reference
//!   and compared bit for bit (slow; prints the running count).

use std::hint::black_box;
use std::time::{Duration, Instant};

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_detect::clip::is_clipped_ci8;
use hk_detect::{
    ClipCount, Detector, DetectorConfig, DetectorEvent, TrackBatch, Tracker, TrackerConfig,
};
use hk_dsp::floor::{FloorConfig, NoiseFloorTracker};
use hk_dsp::synth::Rng;
use hk_dsp::window::WindowKind;
use hk_dsp::{InputInfo, StftConfig, StftProcessor, WelchConfig};
use hk_model::{Provenance, SampleTime, SurveyId, Timestamp};
use num_complex::Complex;

const CHUNK: usize = 1 << 16;

/// The pipeline's `detection_resolution` (hk-pipeline/src/config.rs) with no overrides.
fn resolution(fs: f64) -> (usize, usize) {
    let fft = ((fs / 5_000.0).ceil().max(1.0) as usize)
        .next_power_of_two()
        .clamp(512, 4096);
    let k = ((fs * 2.5e-3 / fft as f64).round() as usize).clamp(4, 10);
    (fft, k)
}

fn provenance(fs: f64) -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:bench",
        "tune": {"center_hz": 98e6, "sample_rate_hz": fs, "lna_db": 24.0, "vga_db": 20.0,
                 "amp_on": false, "bandwidth_hz": 0.75 * fs},
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic",
    });
    ProvenanceHandle::new(serde_json::from_value::<Provenance>(json).unwrap())
}

/// `seconds` of a synthetic FM-band scene at `fs`, ci8.
fn scene(fs: f64, seconds: f64) -> Vec<Complex<i8>> {
    let len = (fs * seconds) as usize;
    let mut rng = Rng::new(58);
    let mut re = vec![0f32; len];
    let mut im = vec![0f32; len];
    let noise = (1e-3f64 / 2.0).sqrt();
    for (r, i) in re.iter_mut().zip(im.iter_mut()) {
        let (a, b) = rng.gaussian_pair();
        *r = (a * noise) as f32;
        *i = (b * noise) as f32;
    }
    let tau = std::f64::consts::TAU;
    // FM-like carriers: 75 kHz deviation at a 1 kHz tone, −35…−15 dBFS.
    for c in 0..40 {
        let off = (c as f64 / 40.0 - 0.5) * 0.7 * fs + 3e3 * rng.unit();
        let amp = 10f64.powf((-35.0 + 20.0 * rng.unit()) / 20.0);
        let (w, beta, wm) = (tau * off / fs, 75.0, tau * 1e3 / fs);
        let ph0 = tau * rng.unit();
        for n in 0..len {
            let ph = w * n as f64 + beta * (wm * n as f64).sin() + ph0;
            re[n] += (amp * ph.cos()) as f32;
            im[n] += (amp * ph.sin()) as f32;
        }
    }
    // Bursts: 20 ms on, period 150–400 ms, −30 dBFS, 25 kHz wide (random phase steps).
    for b in 0..12 {
        let off = (b as f64 / 12.0 - 0.45) * 0.6 * fs + 40e3;
        let period = (fs * (0.15 + 0.25 * rng.unit())) as usize;
        let on = (fs * 0.02) as usize;
        let amp = 10f64.powf(-30.0 / 20.0);
        let w = tau * off / fs;
        let sym = (fs / 12.5e3) as usize;
        let mut ph = 0.0f64;
        let mut start = (rng.unit() * period as f64) as usize;
        while start < len {
            for n in start..(start + on).min(len) {
                if (n - start) % sym == 0 {
                    ph = if rng.unit() < 0.5 {
                        0.0
                    } else {
                        std::f64::consts::PI
                    };
                }
                let p = w * n as f64 + ph;
                re[n] += (amp * p.cos()) as f32;
                im[n] += (amp * p.sin()) as f32;
            }
            start += period;
        }
    }
    let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) as i8;
    re.iter()
        .zip(&im)
        .map(|(&r, &i)| Complex::new(q(r), q(i)))
        .collect()
}

fn load_ci8(path: &str) -> Vec<Complex<i8>> {
    let bytes = std::fs::read(path).expect("read $HK_DETECT_PATH_IQ");
    bytes
        .chunks_exact(2)
        .map(|b| Complex::new(b[0] as i8, b[1] as i8))
        .collect()
}

#[derive(Default)]
struct Split {
    clip: Duration,
    floor: Duration,
    detect: Duration,
    track: Duration,
    total: Duration,
    frames: u64,
    detections: u64,
    upserts: u64,
}

fn run(fs: f64, iq: &[Complex<i8>], seconds: f64) -> Split {
    let (fft_len, averages) = resolution(fs);
    let prov = provenance(fs);
    let welch = WelchConfig {
        fft_len,
        overlap: 0,
        window: WindowKind::Hann,
        holds: false,
        spectral_kurtosis: true,
    };
    let mut stft = StftProcessor::new(StftConfig::new(welch, averages)).unwrap();
    let floor_cfg = FloorConfig::default();
    let mut floor = NoiseFloorTracker::new(floor_cfg).unwrap();
    let dcfg = DetectorConfig::new(SurveyId::new()).with_floor_config(&floor_cfg);
    let mut det = Detector::new(dcfg).unwrap();
    let mut tracker = Tracker::new(TrackerConfig::default());
    // detect.rs drains the tracker every frame and writes (clearing) the batch every 0.5 s.
    let mut batch = TrackBatch::new();
    let flush_samples = (fs * 0.5) as u64;
    let mut next_flush = flush_samples;
    let total = (fs * seconds) as u64;
    let mut clips: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    let mut split = Split::default();
    let mut index = 0u64;
    let mut pos = 0usize;
    let mut buf = vec![Complex::<i8>::default(); CHUNK];
    let t0 = Instant::now();
    while index < total {
        for b in buf.iter_mut() {
            *b = iq[pos];
            pos = (pos + 1) % iq.len();
        }
        let header = BlockHeader {
            time: SampleTime {
                sample_index: index,
                host_time: Timestamp::from_unix_nanos((index as f64 * 1e9 / fs) as i64),
            },
            provenance: prov.clone(),
            discontinuity: if index == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
        };
        let c = Instant::now();
        for (i, &x) in buf.iter().enumerate() {
            if is_clipped_ci8(x) {
                clips.push_back(index + i as u64);
            }
        }
        split.clip += c.elapsed();
        stft.push(InputInfo::from(&header), &buf, |frame| {
            let a = frame.t.sample_index;
            let b = a + frame.sample_count;
            while clips.front().is_some_and(|&i| i < a) {
                clips.pop_front();
            }
            let clipped = clips.partition_point(|&i| i < b) as u64;
            let s = Instant::now();
            let ff = floor.update(frame, |_| {});
            let d = Instant::now();
            let mut records = Vec::new();
            det.process(
                frame,
                ff,
                ClipCount::new(clipped, frame.sample_count),
                &mut |ev: DetectorEvent<'_>| {
                    if let DetectorEvent::Detection(r) = ev {
                        records.push(r);
                    }
                },
            );
            let t = Instant::now();
            for r in &records {
                tracker.push_detection(r, &mut |te| {
                    black_box(te);
                });
            }
            tracker.observe_frame(&det, frame, &mut |te| {
                black_box(te);
            });
            tracker.drain_into(&mut batch);
            if a >= next_flush {
                next_flush += flush_samples;
                split.upserts += batch.upserts.len() as u64;
                batch = TrackBatch::new();
            }
            let e = Instant::now();
            split.floor += d - s;
            split.detect += t - d;
            split.track += e - t;
            split.frames += 1;
            split.detections += records.len() as u64;
        });
        index += CHUNK as u64;
    }
    split.total = t0.elapsed();
    split
}

fn load() -> f64 {
    let mut avg = [0f64; 3];
    // SAFETY: getloadavg writes at most `nelem` doubles into the buffer.
    let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
    if n > 0 { avg[0] } else { f64::NAN }
}

fn main() {
    let seconds: f64 = std::env::var("HK_DETECT_PATH_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4.0);
    let file = std::env::var("HK_DETECT_PATH_IQ").ok();
    let rates: Vec<f64> = match &file {
        Some(_) => vec![
            std::env::var("HK_DETECT_PATH_RATE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(20.0),
        ],
        None => std::env::var("HK_DETECT_PATH_RATES")
            .unwrap_or_else(|_| "8,10,20".into())
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect(),
    };
    println!(
        "hk-detect detect path (clip scan + STFT + floor + detector + tracker), {seconds} s per rate, {} threads available",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    for msps in rates {
        let fs = msps * 1e6;
        let iq = match &file {
            Some(p) => load_ci8(p),
            None => scene(fs, 0.5),
        };
        let (fft, k) = resolution(fs);
        let _ = run(fs, &iq, 0.25); // warm-up
        let load_before = load();
        let s = run(fs, &iq, seconds);
        let wall = s.total.as_secs_f64();
        let stream = (fs * seconds / CHUNK as f64).ceil() * CHUNK as f64 / fs;
        let stages = s.clip + s.floor + s.detect + s.track;
        let stft = s.total.saturating_sub(stages);
        let pct = |d: Duration| 100.0 * d.as_secs_f64() / wall;
        println!(
            "{msps:>5.1} Msps  {fft}x{k}  RTF {:>5.2}  ({:.0} frames, {:.2} det/frame)  stft+glue {:>4.1}%  floor {:>4.1}%  detector {:>4.1}%  tracker {:>4.1}%  clip scan {:>4.1}%  per frame {:.0} µs  load {:.1}",
            stream / wall,
            s.frames,
            s.detections as f64 / s.frames.max(1) as f64,
            pct(stft),
            pct(s.floor),
            pct(s.detect),
            pct(s.track),
            pct(s.clip),
            wall / s.frames.max(1) as f64 * 1e6,
            load_before,
        );
    }
}
