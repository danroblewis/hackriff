//! Shared helpers for the hk-estimate integration tests: provenance, synthetic signals, an
//! independent OBW99 reference and the real-fixture locator.
#![allow(dead_code)]

use std::f64::consts::{PI, TAU};
use std::path::{Path, PathBuf};
use std::process::Command;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::synth::{Rng, complex_noise};
use hk_dsp::{InputInfo, IqSample, WelchConfig, welch};
use hk_estimate::{
    ChannelSnippet, EstimatorConfig, Hints, ParamEstimator, ParameterSet, SnippetConfig,
    SnippetExtractor, SnippetRequest,
};
use hk_model::{Provenance, SampleTime, Timestamp};
use num_complex::{Complex, Complex32};

pub fn provenance_json(center_hz: f64, fs: f64, overload: bool) -> serde_json::Value {
    serde_json::json!({
        "device_id": "synthetic:hk-estimate-test",
        "tune": {
            "center_hz": center_hz, "sample_rate_hz": fs, "lna_db": 16.0, "vga_db": 20.0,
            "amp_on": false, "bandwidth_hz": fs * 0.75,
        },
        "overload": overload, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    })
}

pub fn provenance(center_hz: f64, fs: f64) -> ProvenanceHandle {
    let p: Provenance = serde_json::from_value(provenance_json(center_hz, fs, false)).unwrap();
    ProvenanceHandle::new(p)
}

/// Input metadata for samples starting at stream index `first`.
pub fn info(first: u64, prov: &ProvenanceHandle) -> InputInfo<'_> {
    InputInfo {
        time: SampleTime {
            sample_index: first,
            host_time: Timestamp::from_unix_nanos(1_000_000_000),
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: prov,
    }
}

pub fn db(x: f64) -> f64 {
    10.0 * x.log10()
}

pub fn power(x: &[Complex32]) -> f64 {
    x.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / x.len() as f64
}

pub fn random_bits(rng: &mut Rng, n: usize) -> Vec<u8> {
    (0..n).map(|_| (rng.next_u64() & 1) as u8).collect()
}

/// Unit-power rectangular continuous-phase 2-FSK at baseband (bit 1 = +dev).
pub fn cpfsk(bits: &[u8], fs: f64, rate: f64, dev: f64) -> Vec<Complex32> {
    let sps = fs / rate;
    let n = (bits.len() as f64 * sps).floor() as usize;
    let mut phase = 0.0f64;
    (0..n)
        .map(|i| {
            let b = bits[((i as f64) / sps) as usize];
            let f = if b == 1 { dev } else { -dev };
            let z = Complex32::new(phase.cos() as f32, phase.sin() as f32);
            phase = (phase + TAU * f / fs) % TAU;
            z
        })
        .collect()
}

/// Root-raised-cosine taps (unit energy), `span` symbols each side.
fn rrc(sps: usize, alpha: f64, span: usize) -> Vec<f64> {
    let n = 2 * span * sps + 1;
    let mid = (span * sps) as f64;
    let mut h: Vec<f64> = (0..n)
        .map(|i| {
            let t = (i as f64 - mid) / sps as f64;
            if t.abs() < 1e-9 {
                1.0 - alpha + 4.0 * alpha / PI
            } else if (t.abs() - 1.0 / (4.0 * alpha)).abs() < 1e-9 {
                alpha / 2f64.sqrt()
                    * ((1.0 + 2.0 / PI) * (PI / (4.0 * alpha)).sin()
                        + (1.0 - 2.0 / PI) * (PI / (4.0 * alpha)).cos())
            } else {
                ((PI * t * (1.0 - alpha)).sin() + 4.0 * alpha * t * (PI * t * (1.0 + alpha)).cos())
                    / (PI * t * (1.0 - (4.0 * alpha * t).powi(2)))
            }
        })
        .collect();
    let e: f64 = h.iter().map(|v| v * v).sum::<f64>().sqrt();
    h.iter_mut().for_each(|v| *v /= e);
    h
}

/// Unit-power RRC-shaped BPSK at baseband (`fs / rate` integer).
pub fn bpsk_rrc(rng: &mut Rng, symbols: usize, fs: f64, rate: f64, alpha: f64) -> Vec<Complex32> {
    let sps = (fs / rate).round() as usize;
    let taps = rrc(sps, alpha, 8);
    let n = symbols * sps;
    let mut up = vec![0.0f64; n + taps.len()];
    for s in 0..symbols {
        up[s * sps] = if rng.next_u64() & 1 == 1 { 1.0 } else { -1.0 };
    }
    let mut y = vec![0.0f64; n];
    let d = taps.len() / 2;
    for (i, out) in y.iter_mut().enumerate() {
        let mut acc = 0.0;
        for (m, t) in taps.iter().enumerate() {
            let j = i + d;
            if j >= m && j - m < up.len() {
                acc += up[j - m] * t;
            }
        }
        *out = acc;
    }
    let p = y.iter().map(|v| v * v).sum::<f64>() / n as f64;
    let g = 1.0 / p.sqrt();
    y.iter()
        .map(|&v| Complex32::new((v * g) as f32, 0.0))
        .collect()
}

/// Unit-power complex tone at baseband offset `f` Hz.
pub fn tone(len: usize, f: f64, fs: f64) -> Vec<Complex32> {
    (0..len)
        .map(|n| {
            let ph = TAU * f * n as f64 / fs;
            Complex32::new(ph.cos() as f32, ph.sin() as f32)
        })
        .collect()
}

/// Independent OBW99 of a clean signal: Hann Welch (8192 bins), 0.5 % / 99.5 % points.
pub fn obw99_reference(x: &[Complex32], fs: f64) -> f64 {
    let nfft = 8192.min(x.len().next_power_of_two() / 2);
    let s = welch(x, fs, 0.0, &WelchConfig::new(nfft)).unwrap();
    let p: Vec<f64> = s.psd.iter().map(|&v| f64::from(v)).collect();
    let total: f64 = p.iter().sum();
    let mut acc = 0.0;
    let mut lo = 0;
    while acc + p[lo] < 0.005 * total {
        acc += p[lo];
        lo += 1;
    }
    let mut acc = 0.0;
    let mut hi = p.len() - 1;
    while acc + p[hi] < 0.005 * total {
        acc += p[hi];
        hi -= 1;
    }
    (hi - lo + 1) as f64 * fs / nfft as f64
}

/// A synthetic capture: `signal` (unit power) scaled to `snr_db` in `obw_hz`, at `offset_hz`,
/// between `pre` and `post` samples of noise of variance `noise_var`.
pub struct Scene {
    pub iq: Vec<Complex32>,
    pub fs: f64,
    pub noise_var: f64,
    pub signal_power: f64,
    pub start: usize,
    pub len: usize,
    pub offset_hz: f64,
}

impl Scene {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        signal: &[Complex32],
        fs: f64,
        offset_hz: f64,
        snr_db: f64,
        obw_hz: f64,
        pre: usize,
        post: usize,
        seed: u64,
    ) -> Self {
        let noise_var = 1e-4;
        let n0 = noise_var / fs;
        let signal_power = 10f64.powf(snr_db / 10.0) * n0 * obw_hz;
        let mut rng = Rng::new(seed);
        let mut iq = complex_noise(&mut rng, pre + signal.len() + post, noise_var);
        let a = signal_power.sqrt();
        for (i, s) in signal.iter().enumerate() {
            let n = pre + i;
            let ph = TAU * (offset_hz / fs * n as f64).fract();
            iq[n] += s * Complex32::new((a * ph.cos()) as f32, (a * ph.sin()) as f32);
        }
        Self {
            iq,
            fs,
            noise_var,
            signal_power,
            start: pre,
            len: signal.len(),
            offset_hz,
        }
    }

    pub fn n0(&self) -> f64 {
        self.noise_var / self.fs
    }

    /// True SNR in `band_hz`, dB.
    pub fn snr_db_in(&self, band_hz: f64) -> f64 {
        db(self.signal_power / (self.n0() * band_hz))
    }

    /// A box request `[start, start+len)` centred at `offset + center_error`, `bw` wide.
    pub fn request(&self, center_error_hz: f64, bw: f64) -> SnippetRequest {
        SnippetRequest {
            start_index: self.start as u64,
            end_index: (self.start + self.len) as u64,
            center_offset_hz: self.offset_hz + center_error_hz,
            bandwidth_hz: bw,
        }
    }
}

pub const CENTER_HZ: f64 = 100e6;

/// Extract + estimate with default configs.
pub fn run<T: IqSample>(
    iq: &[T],
    fs: f64,
    request: &SnippetRequest,
    hints: &Hints,
) -> (ChannelSnippet, ParameterSet) {
    let prov = provenance(CENTER_HZ, fs);
    let mut ex = SnippetExtractor::new(SnippetConfig::default());
    let snip = ex.extract(info(0, &prov), iq, request).expect("extract");
    let mut est = ParamEstimator::new(EstimatorConfig::default());
    let ps = est.estimate(&snip, hints);
    (snip, ps)
}

// ------------------------------------------------------------------------ real fixtures

/// Environment variable: `1` makes a missing real fixture a failure instead of a skip.
pub const REQUIRE_FIXTURES_ENV: &str = "HK_E2E_REQUIRE_FIXTURES";

fn is_real_data(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if meta.len() < 1024 {
        return false; // a Git LFS pointer
    }
    true
}

/// `(meta, data)` for a 2026-09-13 HackRF fixture. The data comes from this checkout, from
/// `$HK_FIXTURE_DATA_ROOT/<same relative path>`, or from the main checkout of a git worktree
/// (worktrees often hold only LFS pointers).
pub fn fixture_paths(name: &str) -> Option<(PathBuf, PathBuf)> {
    let root = hk_e2e::paths::repo_root();
    let rel = format!("fixtures/hackrf/2026-09-13/{name}");
    let meta = root.join(format!("{rel}.sigmf-meta"));
    let mut candidates = vec![root.join(format!("{rel}.sigmf-data"))];
    if let Some(dir) = std::env::var_os("HK_FIXTURE_DATA_ROOT") {
        candidates.push(PathBuf::from(dir).join(format!("{rel}.sigmf-data")));
    }
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(&root)
        .output()
    {
        if out.status.success() {
            let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
            if let Some(main) = common.parent() {
                candidates.push(main.join(format!("{rel}.sigmf-data")));
            }
        }
    }
    let data = candidates.into_iter().find(|p| is_real_data(p))?;
    meta.is_file().then_some((meta, data))
}

/// Resolves a fixture or returns from the test with a SKIP message.
#[macro_export]
macro_rules! fixture_or_skip {
    ($name:expr) => {
        match common::fixture_paths($name) {
            Some(p) => p,
            None => {
                if std::env::var(common::REQUIRE_FIXTURES_ENV).is_ok_and(|v| v == "1") {
                    panic!("fixture {} data not found (Git LFS not fetched?)", $name);
                }
                eprintln!("SKIP {}: fixture {} data not found", module_path!(), $name);
                return;
            }
        }
    };
}

pub fn read_ci8(path: &Path) -> Vec<Complex<i8>> {
    std::fs::read(path)
        .expect("read .sigmf-data")
        .chunks_exact(2)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect()
}

/// The global `hackriff:provenance` of a `.sigmf-meta`.
pub fn meta_provenance(meta: &Path) -> ProvenanceHandle {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(meta).unwrap()).unwrap();
    let p: Provenance = serde_json::from_value(v["global"]["hackriff:provenance"].clone()).unwrap();
    ProvenanceHandle::new(p)
}
