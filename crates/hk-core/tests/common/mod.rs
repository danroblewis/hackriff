//! Shared helpers for hk-core integration tests: deterministic RNG, SigMF fixture writing,
//! checksums.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hk_core::{BlockHeader, Source};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{ClockSource, Provenance, TimestampMethod, Tune};
use num_complex::Complex32;

/// xorshift64* — deterministic, dependency-free.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in (0, 1].
    pub fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) + 1) as f64 / (1u64 << 53) as f64
    }

    /// A pair of independent standard normal values (Box–Muller).
    pub fn gaussian_pair(&mut self) -> (f64, f64) {
        let (u1, u2) = (self.unit(), self.unit());
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = std::f64::consts::TAU * u2;
        (r * theta.cos(), r * theta.sin())
    }
}

/// A unique scratch directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "hk-core-{tag}-{}-{n}-{}",
            std::process::id(),
            hk_model::Timestamp::now().as_unix_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn provenance(device_id: &str, center_hz: f64, sample_rate_hz: f64) -> Provenance {
    Provenance {
        device_id: device_id.into(),
        tune: Tune {
            center_hz,
            sample_rate_hz,
            lna_db: 24.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: sample_rate_hz * 0.75,
        },
        clip_count: 0,
        overload: false,
        temperature_c: None,
        antenna_port: None,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: Some(0),
    }
}

pub fn capture(sample_start: u64, frequency: f64) -> Capture {
    Capture {
        sample_start,
        frequency: Some(frequency),
        datetime: None,
        provenance: None,
        extra: Default::default(),
    }
}

/// Writes `<dir>/<name>.sigmf-meta` + `.sigmf-data`; returns the meta path.
pub fn write_recording(dir: &Path, name: &str, meta: &SigmfMeta, data: &[u8]) -> PathBuf {
    let meta_path = dir.join(format!("{name}.sigmf-meta"));
    meta.write(&meta_path).unwrap();
    std::fs::write(hk_model::sigmf::data_path_for(&meta_path), data).unwrap();
    meta_path
}

/// Interleaved ci8 bytes of a counter ramp: sample k = (k mod 256, -(k mod 256)).
pub fn ramp_ci8(n: usize) -> Vec<u8> {
    (0..n)
        .flat_map(|k| {
            let v = (k % 256) as u8;
            [v, v.wrapping_neg()]
        })
        .collect()
}

pub fn meta(datatype: Datatype, sample_rate: f64) -> SigmfMeta {
    let mut m = SigmfMeta::new(datatype);
    m.global.sample_rate = Some(sample_rate);
    m
}

/// FNV-1a over bytes.
pub struct Fnv(u64);

impl Fnv {
    pub fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    pub fn samples(&mut self, samples: &[Complex32]) {
        for s in samples {
            self.bytes(&s.re.to_bits().to_le_bytes());
            self.bytes(&s.im.to_bits().to_le_bytes());
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// Drains a source: returns every header with its block length, plus a checksum over samples,
/// counters, times and flags (not provenance ids, which are fresh UUIDs per run).
pub fn drain(source: &mut dyn Source) -> (Vec<(BlockHeader, usize)>, u64) {
    let mut fnv = Fnv::new();
    let mut headers = Vec::new();
    let mut buf = Vec::with_capacity(1 << 16);
    while let Some(h) = source.read_block(&mut buf).unwrap() {
        fnv.u64(h.time.sample_index);
        fnv.u64(h.time.host_time.as_unix_nanos() as u64);
        fnv.u64(u64::from(h.discontinuity.bits()));
        fnv.u64(h.dropped_before);
        fnv.samples(&buf);
        headers.push((h, buf.len()));
    }
    (headers, fnv.finish())
}
