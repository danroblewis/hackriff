//! Ring-buffer throughput and live attach/detach bench.
//!
//! ```text
//! cargo run --release -p hk-core --example ring_throughput [seconds-per-run]
//! ```
//!
//! 1. Unpaced writer + 2 always-on readers (Complex32 and Complex<i8>).
//! 2. Highest paced rate with zero loss for writer + 2 readers.
//! 3. 20 Msps Complex32 with 2 always-on readers while 8 chain threads attach/detach readers
//!    for 800 cycles total: worst attach time and loss on every reader.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hk_core::rt::{PriorityOutcome, spawn_capture_thread};
use hk_core::{
    BlockHeader, Discontinuity, ProvenanceHandle, ReadOutcome, RingConfig, RingHandle, RingSample,
    RingWriter, ring_buffer,
};
use hk_model::{ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
use num_complex::{Complex, Complex32};

const BLOCK: usize = 65_536;
const RING: RingConfig = RingConfig {
    sample_capacity: 1 << 24,
    block_capacity: 1 << 12,
};

fn provenance(fs: f64) -> ProvenanceHandle {
    ProvenanceHandle::new(Provenance {
        device_id: "synthetic:bench".into(),
        tune: Tune {
            center_hz: 100e6,
            sample_rate_hz: fs,
            lna_db: 16.0,
            vga_db: 20.0,
            amp_on: false,
            bandwidth_hz: fs,
        },
        quantisation_limited: false,
        noise_sigma_lsb: None,
        overload: false,
        temperature_c: None,
        antenna_port: None,
        bias_tee: hk_model::BiasTee::Unknown,
        clock_source: ClockSource::Internal,
        clock_locked: true,
        calibration_state_ref: None,
        spur_mask_ref: None,
        timestamp_method: TimestampMethod::Synthetic,
        timestamp_error_budget_ns: Some(0),
        capture_artefacts: Vec::new(),
    })
}

struct WriterStats {
    written: u64,
    elapsed: f64,
    max_push: Duration,
    priority: PriorityOutcome,
}

/// Pushes blocks at `rate` samples/s (unpaced if `None`) until `stop`; drops the writer at the end.
fn spawn_writer<T: RingSample>(
    mut writer: RingWriter<T>,
    sample: T,
    rate: Option<f64>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<WriterStats> {
    spawn_capture_thread("hk-bench-capture", move |priority| {
        let samples = vec![sample; BLOCK];
        let prov = provenance(rate.unwrap_or(20e6));
        let start = Instant::now();
        let mut first = 0u64;
        let mut max_push = Duration::ZERO;
        while !stop.load(Ordering::Relaxed) {
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: first,
                    host_time: Timestamp::UNIX_EPOCH,
                },
                provenance: prov.clone(),
                discontinuity: Discontinuity::NONE,
                dropped_before: 0,
            };
            let t = Instant::now();
            writer.push(&header, &samples).unwrap();
            max_push = max_push.max(t.elapsed());
            first += BLOCK as u64;
            if let Some(rate) = rate {
                let due = Duration::from_secs_f64(first as f64 / rate);
                if let Some(sleep) = due.checked_sub(start.elapsed()) {
                    thread::sleep(sleep);
                }
            }
        }
        WriterStats {
            written: first,
            elapsed: start.elapsed().as_secs_f64(),
            max_push,
            priority,
        }
    })
    .unwrap()
}

#[derive(Default)]
struct ReaderStats {
    read: u64,
    dropped: u64,
    overruns: u64,
}

/// An always-on reader: consumes until the ring closes, touching every sample.
fn spawn_reader<T: RingSample>(ring: &RingHandle<T>) -> JoinHandle<ReaderStats> {
    let mut reader = ring.reader();
    thread::spawn(move || {
        let mut buf = vec![T::default(); BLOCK];
        while !matches!(
            reader.read_timeout(&mut buf, Duration::from_millis(50)),
            ReadOutcome::Closed
        ) {}
        ReaderStats {
            read: reader.samples_read(),
            dropped: reader.lost_samples(),
            overruns: reader.overruns(),
        }
    })
}

fn run<T: RingSample>(sample: T, rate: Option<f64>, secs: f64) -> (WriterStats, Vec<ReaderStats>) {
    let (writer, ring) = ring_buffer::<T>(RING);
    let readers: Vec<_> = (0..2).map(|_| spawn_reader(&ring)).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let w = spawn_writer(writer, sample, rate, Arc::clone(&stop));
    thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    let w = w.join().unwrap();
    let r = readers.into_iter().map(|h| h.join().unwrap()).collect();
    (w, r)
}

fn msps(samples: u64, secs: f64) -> f64 {
    samples as f64 / secs / 1e6
}

fn bench_type<T: RingSample>(label: &str, sample: T, secs: f64) {
    let (w, readers) = run(sample, None, secs);
    println!(
        "[{label}] unpaced: writer {:.0} Msps (priority {:?})",
        msps(w.written, w.elapsed),
        w.priority
    );
    for (i, r) in readers.iter().enumerate() {
        println!(
            "[{label}]   reader {i}: {:.0} Msps delivered, {} samples dropped in {} overruns",
            msps(r.read, w.elapsed),
            r.dropped,
            r.overruns
        );
    }
    let mut best = None;
    for rate in [20e6, 40e6, 80e6, 160e6, 320e6, 640e6, 1280e6] {
        let (w, readers) = run(sample, Some(rate), secs);
        let lossless = readers
            .iter()
            .all(|r| r.dropped == 0 && r.read == w.written);
        let achieved = msps(w.written, w.elapsed);
        println!(
            "[{label}] paced {:>5.0} Msps: achieved {achieved:.0} Msps, 2 readers lossless={lossless}, max push {:?}",
            rate / 1e6,
            w.max_push
        );
        if !lossless || achieved < 0.95 * rate / 1e6 {
            break;
        }
        best = Some(achieved);
    }
    if let Some(best) = best {
        println!("[{label}] sustained lossless writer + 2 readers: >= {best:.0} Msps");
    }
}

fn attach_detach(secs_hint: f64) {
    const CHAINS: usize = 8;
    const CYCLES: usize = 800;
    let (writer, ring) = ring_buffer::<Complex32>(RING);
    let always_on: Vec<_> = (0..2).map(|_| spawn_reader(&ring)).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let w = spawn_writer(
        writer,
        Complex32::new(0.25, -0.5),
        Some(20e6),
        Arc::clone(&stop),
    );
    let dwell = Duration::from_secs_f64((secs_hint / 100.0).clamp(0.005, 0.05));
    let chains: Vec<_> = (0..CHAINS)
        .map(|_| {
            let ring = ring.clone();
            thread::spawn(move || {
                let mut worst = Duration::ZERO;
                let mut stats = ReaderStats::default();
                let mut buf = vec![Complex32::default(); 16_384];
                for _ in 0..CYCLES / CHAINS {
                    let t = Instant::now();
                    let mut reader = ring.reader();
                    worst = worst.max(t.elapsed());
                    let until = Instant::now() + dwell;
                    let mut power = 0.0f64;
                    while Instant::now() < until {
                        if let ReadOutcome::Data(c) =
                            reader.read_timeout(&mut buf, Duration::from_millis(5))
                        {
                            power += buf[..c.len]
                                .iter()
                                .map(|s| f64::from(s.norm_sqr()))
                                .sum::<f64>();
                        }
                    }
                    std::hint::black_box(power);
                    stats.read += reader.samples_read();
                    stats.dropped += reader.lost_samples();
                    stats.overruns += reader.overruns();
                }
                (worst, stats)
            })
        })
        .collect();
    let results: Vec<_> = chains.into_iter().map(|h| h.join().unwrap()).collect();
    stop.store(true, Ordering::Relaxed);
    let w = w.join().unwrap();
    let worst = results.iter().map(|(d, _)| *d).max().unwrap();
    let chain_dropped: u64 = results.iter().map(|(_, s)| s.dropped).sum();
    let chain_read: u64 = results.iter().map(|(_, s)| s.read).sum();
    println!(
        "[attach/detach] 20 Msps Complex32, {CHAINS} chains x {} cycles ({:?} dwell): worst attach {:?}, chain samples {} dropped {}, writer max push {:?}, achieved {:.1} Msps",
        CYCLES / CHAINS,
        dwell,
        worst,
        chain_read,
        chain_dropped,
        w.max_push,
        msps(w.written, w.elapsed)
    );
    for (i, h) in always_on.into_iter().enumerate() {
        let r = h.join().unwrap();
        println!(
            "[attach/detach]   always-on reader {i}: read {} of {} written, dropped {}",
            r.read, w.written, r.dropped
        );
    }
}

fn main() {
    let secs: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);
    println!(
        "ring_throughput: blocks of {BLOCK} samples, ring 2^24 samples, {secs} s per run, {} CPUs",
        thread::available_parallelism().map_or(0, |n| n.get())
    );
    bench_type("Complex32", Complex32::new(0.5, -0.5), secs);
    bench_type("Complex<i8>", Complex::new(64i8, -64i8), secs);
    attach_detach(secs);
}
