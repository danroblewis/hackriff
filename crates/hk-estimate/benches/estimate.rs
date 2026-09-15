//! Cost of one C13 [`ParamEstimator::estimate`] call on the analog chain's probe (T-081): a
//! 0.5 s, 200 kHz box (snippet rate 500 kHz, as `hk-pipeline`'s WFM/RDS chain extracts it).
//!
//! Run with `cargo bench -p hk-estimate --bench estimate`. Prints, per scene, the median and
//! minimum wall time per call over `$HK_ESTIMATE_BENCH_RUNS` calls (default 7) and the 1-minute
//! load average (other processes share the machine).
//!
//! - `narrow-carrier`: an unmodulated carrier 2 kHz off the box centre at 40 dB SNR. Its OBW99
//!   is a few hundred Hz, so the channel filter is at its narrowest (the dense urban replay's
//!   1.4 s call before T-081).
//! - `wfm`: stereo-pilot FM (±75 kHz, 1 kHz tone + 19 kHz pilot) at 25 dB SNR.
//! - `noise`: the box holds only noise.

use std::f64::consts::TAU;
use std::hint::black_box;
use std::time::Instant;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::InputInfo;
use hk_dsp::synth::{Rng, complex_noise};
use hk_estimate::{
    ChannelSnippet, EstimatorConfig, Hints, ParamEstimator, SnippetConfig, SnippetExtractor,
    SnippetRequest,
};
use hk_model::{Provenance, SampleTime, Timestamp};
use num_complex::Complex32;

const FS: f64 = 2e6;
const SECONDS: f64 = 0.5;

fn provenance() -> ProvenanceHandle {
    let p: Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:hk-estimate-bench",
        "tune": {
            "center_hz": 100e6, "sample_rate_hz": FS, "lna_db": 16.0, "vga_db": 20.0,
            "amp_on": false, "bandwidth_hz": FS * 0.75,
        },
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .expect("provenance");
    ProvenanceHandle::new(p)
}

/// Noise of variance 1e-4 plus `signal(t)` scaled to `snr_db` in `band_hz`.
fn scene(snr_db: f64, band_hz: f64, signal: impl Fn(f64) -> Complex32) -> Vec<Complex32> {
    let pad = (0.01 * FS) as usize;
    let n = (SECONDS * FS) as usize + 2 * pad;
    let noise_var = 1e-4;
    let a = (10f64.powf(snr_db / 10.0) * noise_var / FS * band_hz).sqrt() as f32;
    let mut iq = complex_noise(&mut Rng::new(81), n, noise_var);
    if a > 0.0 {
        for (i, v) in iq.iter_mut().enumerate().skip(pad).take(n - 2 * pad) {
            *v += signal(i as f64 / FS) * a;
        }
    }
    iq
}

fn snippet(iq: &[Complex32], prov: &ProvenanceHandle) -> ChannelSnippet {
    let pad = (0.01 * FS) as u64;
    let info = InputInfo {
        time: SampleTime {
            sample_index: 0,
            host_time: Timestamp::from_unix_nanos(1_000_000_000),
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: prov,
    };
    let request = SnippetRequest {
        start_index: pad,
        end_index: pad + (SECONDS * FS) as u64,
        center_offset_hz: 300e3,
        bandwidth_hz: 200e3,
    };
    SnippetExtractor::new(SnippetConfig::default())
        .extract(info, iq, &request)
        .expect("extract")
}

fn load_average() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .or_else(|| std::fs::read_to_string("/proc/loadavg").ok())
        .map_or_else(|| "?".into(), |s| s.trim().to_owned())
}

fn main() {
    let runs: usize = std::env::var("HK_ESTIMATE_BENCH_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7)
        .max(1);
    let prov = provenance();
    let off = 300e3;
    let scenes: [(&str, Vec<Complex32>); 3] = [
        (
            "narrow-carrier",
            scene(40.0, 1e3, |t| {
                Complex32::from_polar(1.0, (TAU * (off + 2e3) * t).rem_euclid(TAU) as f32)
            }),
        ),
        (
            "wfm",
            scene(25.0, 200e3, |t| {
                let mpx_phase = 75e3 / 1e3 * 0.9 * (TAU * 1e3 * t).sin()
                    + 75e3 / 19e3 * 0.1 * (TAU * 19e3 * t).sin();
                Complex32::from_polar(1.0, (TAU * off * t + mpx_phase).rem_euclid(TAU) as f32)
            }),
        ),
        (
            "noise",
            scene(f64::NEG_INFINITY, 200e3, |_| Complex32::default()),
        ),
    ];
    println!("hk-estimate estimate bench: 0.5 s, 200 kHz box, {runs} calls per scene");
    println!("load average: {}", load_average());
    for (name, iq) in &scenes {
        let snip = snippet(iq, &prov);
        let mut est = ParamEstimator::new(EstimatorConfig::default());
        let mut ms = Vec::with_capacity(runs);
        let mut last = None;
        for _ in 0..runs {
            let t0 = Instant::now();
            let p = est.estimate(black_box(&snip), &Hints::default());
            ms.push(t0.elapsed().as_secs_f64() * 1e3);
            last = Some(p);
        }
        ms.sort_by(f64::total_cmp);
        let p = last.expect("runs >= 1");
        println!(
            "{name:<15} snippet {} samples @ {:.0} Hz: median {:.1} ms, min {:.1} ms | obw99 {:?} \
             snr {:?} cfo {:?}",
            snip.samples.len(),
            snip.sample_rate_hz,
            ms[ms.len() / 2],
            ms[0],
            p.obw99_hz.value().map(|v| v.round()),
            p.snr_box_db.value().map(|v| (v * 10.0).round() / 10.0),
            p.cfo_hz.value().map(|v| v.round()),
        );
    }
    println!("load average: {}", load_average());
}
