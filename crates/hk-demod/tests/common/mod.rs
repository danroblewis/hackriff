//! Shared helpers for the hk-demod acceptance tests: provenance, synthetic analog signals and the
//! real-fixture locator (same rules as the hk-estimate tests).
#![allow(dead_code)]

use std::f64::consts::TAU;
use std::path::{Path, PathBuf};
use std::process::Command;

use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::InputInfo;
use hk_dsp::synth::{Rng, complex_noise};
use hk_model::{Provenance, SampleTime, Timestamp};
use num_complex::{Complex, Complex32};

pub const SIGNAL_062: &str = "SIGNAL-062";

pub fn provenance(center_hz: f64, fs: f64) -> ProvenanceHandle {
    let p: Provenance = serde_json::from_value(serde_json::json!({
        "device_id": "synthetic:hk-demod-test",
        "tune": {
            "center_hz": center_hz, "sample_rate_hz": fs, "lna_db": 16.0, "vga_db": 20.0,
            "amp_on": false, "bandwidth_hz": fs * 0.75,
        },
        "overload": false, "quantisation_limited": false, "clock_source": "internal",
        "clock_locked": true, "timestamp_method": "synthetic", "timestamp_error_budget_ns": 0,
    }))
    .unwrap();
    ProvenanceHandle::new(p)
}

/// Input metadata for samples starting at stream index `first`.
pub fn info(first: u64, prov: &ProvenanceHandle) -> InputInfo<'_> {
    InputInfo {
        time: SampleTime {
            sample_index: first,
            host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: prov,
    }
}

fn noise(len: usize, seed: u64, variance: f64) -> Vec<Complex32> {
    complex_noise(&mut Rng::new(seed), len, variance)
}

/// AM: `a·(1 + m·cos 2π f_mod t)` at `offset_hz`, plus noise.
pub fn am_tone(fs: f64, secs: f64, offset_hz: f64, a: f64, m: f64, f_mod: f64) -> Vec<Complex32> {
    let n = (fs * secs) as usize;
    let mut x = noise(n, 31, 1e-5);
    for (k, s) in x.iter_mut().enumerate() {
        let t = k as f64 / fs;
        let env = a * (1.0 + m * (TAU * f_mod * t).cos());
        let ph = TAU * offset_hz * t;
        *s += Complex32::new((env * ph.cos()) as f32, (env * ph.sin()) as f32);
    }
    x
}

/// NBFM: tone `f_mod` at deviation `dev_hz`, amplitude `a`, at `offset_hz`, plus noise.
pub fn nbfm_tone(
    fs: f64,
    secs: f64,
    offset_hz: f64,
    a: f64,
    dev_hz: f64,
    f_mod: f64,
) -> Vec<Complex32> {
    let n = (fs * secs) as usize;
    let mut x = noise(n, 32, 1e-5);
    for (k, s) in x.iter_mut().enumerate() {
        let t = k as f64 / fs;
        let ph = TAU * offset_hz * t + dev_hz / f_mod * (TAU * f_mod * t).sin();
        *s += Complex32::new((a * ph.cos()) as f32, (a * ph.sin()) as f32);
    }
    x
}

/// Pure complex noise.
pub fn noise_only(fs: f64, secs: f64) -> Vec<Complex32> {
    noise((fs * secs) as usize, 33, 1e-5)
}

// ------------------------------------------------------------------------ real fixtures

/// `1` makes a missing real fixture a failure instead of a skip.
pub const REQUIRE_FIXTURES_ENV: &str = "HK_E2E_REQUIRE_FIXTURES";

fn is_real_data(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.len() >= 1024) // smaller: a Git LFS pointer
}

/// `(meta, data)` for a 2026-09-13 HackRF fixture: this checkout, `$HK_FIXTURE_DATA_ROOT`, or
/// the main checkout of a git worktree.
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
        && out.status.success()
    {
        let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        if let Some(main) = common.parent() {
            candidates.push(main.join(format!("{rel}.sigmf-data")));
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
