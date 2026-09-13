//! Spectrum-history ingest throughput and storage rate (T-017).
//!
//! Run with `cargo bench -p hk-store --bench history_ingest`. Folds 4096-bin frames (a 20 Msps
//! dwell, 4.88 kHz bins, 30 frames/s) into the default pyramid (6.25 kHz × 1 s level-0 cells) and
//! prints single-thread frames/s, including tile sealing, rollup and writes. Then ingests one
//! simulated hour and prints bytes on disk per hour per level.

use std::hint::black_box;
use std::time::{Duration, Instant};

use hk_model::{PowerUnit, Timestamp};
use hk_store::{FrameInput, Pyramid, PyramidConfig};

const BINS: usize = 4096;
const FS: f64 = 20e6;
const CENTER: f64 = 433.92e6;
const FPS: i64 = 30;

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// `n` distinct frames: K = 16 averaged noise at −140 dBFS/Hz plus a few carriers.
fn frames(n: usize) -> Vec<(Vec<f32>, Vec<f32>)> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let floor = 1e-14f64;
    (0..n)
        .map(|k| {
            let mut psd = vec![0f32; BINS];
            let mut peak = vec![0f32; BINS];
            for (p, m) in psd.iter_mut().zip(&mut peak) {
                let mut sum = 0.0;
                let mut max = 0f64;
                for _ in 0..16 {
                    let e = -rng.unit().max(1e-12).ln();
                    sum += e;
                    max = max.max(e);
                }
                *p = (floor * sum / 16.0) as f32;
                *m = (floor * max) as f32;
            }
            for c in [300usize, 1500, 2900 + (k % 7)] {
                psd[c] = 1e-10;
                peak[c] = 2e-10;
            }
            (psd, peak)
        })
        .collect()
}

fn frame<'a>(i: i64, data: &'a [(Vec<f32>, Vec<f32>)], t0: i64) -> FrameInput<'a> {
    let (psd, peak) = &data[i as usize % data.len()];
    let mut f = FrameInput::new(
        Timestamp::from_unix_nanos(t0 + i * 1_000_000_000 / FPS),
        1_000_000_000 / FPS,
        CENTER - FS / 2.0,
        FS / BINS as f64,
        PowerUnit::Dbfs,
        psd,
    );
    f.peak = Some(peak);
    f
}

fn main() {
    let dir = std::env::temp_dir().join(format!("hk-store-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let data = frames(64);
    let t0 = 1_789_300_800_000_000_000i64; // 2026-09-13T12:00:00Z

    // Throughput.
    {
        let mut p = Pyramid::open(&dir, PyramidConfig::default()).unwrap();
        let mut i = 0i64;
        while i < 90 * FPS {
            p.ingest(&frame(i, &data, t0)).unwrap(); // warm-up: 90 s (seals a minute)
            i += 1;
        }
        let start = Instant::now();
        let first = i;
        while start.elapsed() < Duration::from_secs(5) {
            for _ in 0..FPS {
                black_box(p.ingest(&frame(i, &data, t0)).unwrap());
                i += 1;
            }
        }
        let fps = (i - first) as f64 / start.elapsed().as_secs_f64();
        println!(
            "ingest: {fps:.0} frames/s ({:.1}x real time at {FPS} frames/s), {BINS} bins -> {} L0 cells/frame",
            fps / FPS as f64,
            (FS / 6250.0) as usize
        );
    }
    let _ = std::fs::remove_dir_all(&dir);

    // Storage rate: one simulated hour.
    let mut p = Pyramid::open(&dir, PyramidConfig::default()).unwrap();
    let hour = 3600 * FPS;
    let start = Instant::now();
    for i in 0..hour {
        p.ingest(&frame(i, &data, t0)).unwrap();
    }
    p.seal_through(Timestamp::from_unix_nanos(t0 + 3_600_000_000_000))
        .unwrap();
    let wall = start.elapsed().as_secs_f64();
    println!("one simulated hour ingested in {wall:.1} s");
    for level in 0..p.geometry().n_levels() {
        println!(
            "  L{level}: {:>10} bytes sealed ({} tiles)",
            p.level_bytes(level),
            p.sealed_keys(level).len()
        );
    }
    println!(
        "total on disk after 1 h: {:.1} MB (20 MHz dwell, default scheme)",
        p.disk_bytes() as f64 / 1e6
    );
    drop(p);
    let _ = std::fs::remove_dir_all(&dir);
}
