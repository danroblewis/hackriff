//! Spike S1 (owned mini-dataflow): live attach/detach of DDC+FM chains on a
//! running 20 Msps capture, with sample-counter continuity checks.
//!
//! Topology:
//!   source thread --publish--> Ring<i8> (history of Arc<Block>)
//!        ├── always-on reader: i8->c32 -> FFT 4096 -> PSD accumulator
//!        └── N dynamically attached chain readers (thread each):
//!              i8->c32 -> [Box<dyn Node>]* (DDC -> FM discriminator -> stats sink)
//!
//! Chains are built from data (`ChainSpec`), so adding a new chain never
//! needs a rebuild or a capture restart.

#[path = "../../common/dsp.rs"]
mod dsp;
mod ring;
#[path = "../../common/synth.rs"]
mod synth;

use num_complex::Complex32;
use ring::{Next, Reader, Ring, Writer};
use rustfft::FftPlanner;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use synth::{BLOCK, FIFO_CAP, FS};

const FFT_LEN: usize = 4096;

// ---------------------------------------------------------------- nodes ---

/// Buffers flowing between nodes.
enum Buf {
    C32(Vec<Complex32>),
    F32(Vec<f32>),
    None,
}

/// A processing node. Chains are `Vec<Box<dyn Node>>` assembled at runtime.
trait Node: Send {
    fn work(&mut self, input: &Buf, output: &mut Buf);
    fn stats(&self) -> Option<dsp::DemodStats> {
        None
    }
}

struct DdcNode(dsp::XlatingDecimator);
impl Node for DdcNode {
    fn work(&mut self, input: &Buf, output: &mut Buf) {
        let (Buf::C32(i), Buf::C32(o)) = (input, output) else { panic!("DDC wants c32->c32") };
        o.clear();
        self.0.process(i, o);
    }
}

struct FmNode(dsp::FmDiscriminator);
impl Node for FmNode {
    fn work(&mut self, input: &Buf, output: &mut Buf) {
        let (Buf::C32(i), Buf::F32(o)) = (input, output) else { panic!("FM wants c32->f32") };
        o.clear();
        self.0.process(i, o);
    }
}

struct StatsSink(dsp::DemodStats);
impl Node for StatsSink {
    fn work(&mut self, input: &Buf, _output: &mut Buf) {
        let Buf::F32(i) = input else { panic!("sink wants f32") };
        self.0.push(i);
    }
    fn stats(&self) -> Option<dsp::DemodStats> {
        Some(self.0)
    }
}

/// Build a chain from a data description. In the product this is where a
/// chain spec from the API/scheduler (or a plugin manifest) is interpreted.
fn build_chain(spec: &dsp::ChainSpec) -> (Vec<Box<dyn Node>>, Vec<Buf>) {
    let ddc = dsp::XlatingDecimator::new(spec.offset_hz, spec.decim, FS);
    let fs_out = ddc.out_rate(FS);
    let nodes: Vec<Box<dyn Node>> = vec![
        Box::new(DdcNode(ddc)),
        Box::new(FmNode(dsp::FmDiscriminator::new(fs_out))),
        Box::new(StatsSink(dsp::DemodStats::default())),
    ];
    // output buffer of node k
    let bufs = vec![Buf::C32(Vec::new()), Buf::F32(Vec::new()), Buf::None];
    (nodes, bufs)
}

fn run_nodes(nodes: &mut [Box<dyn Node>], input: &Buf, bufs: &mut [Buf]) {
    for k in 0..nodes.len() {
        let (before, after) = bufs.split_at_mut(k);
        let inp = if k == 0 { input } else { &before[k - 1] };
        nodes[k].work(inp, &mut after[0]);
    }
}

// --------------------------------------------------------------- source ---

#[derive(Default)]
struct SourceReport {
    blocks: u64,
    samples: u64,
    dropped_samples: u64,
    drop_events: u64,
    /// (seconds since source start, dropped samples, lag ms) per drop event
    drop_log: Vec<(f64, u64, f64)>,
    late_blocks_1ms: u64,
    max_lag_ms: f64,
    allocations: u64,
    cpu_s: f64,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// Raise the calling thread to QOS_CLASS_USER_INTERACTIVE (macOS). In the
/// product the capture thread would get this (or SCHED_FIFO on the Jetson).
fn raise_thread_priority() {
    #[cfg(target_os = "macos")]
    unsafe {
        pthread_set_qos_class_self_np(0x21, 0);
    }
}

fn spawn_source(
    mut w: Writer<i8>,
    table: Arc<Vec<i8>>,
    stop: Arc<AtomicBool>,
    unpaced: bool,
    qos: bool,
) -> JoinHandle<SourceReport> {
    thread::Builder::new()
        .name("source".into())
        .spawn(move || {
            if qos {
                raise_thread_priority();
            }
            let cpu0 = dsp::thread_cpu_s();
            let mut rep = SourceReport::default();
            let t0 = Instant::now();
            let mut next_sample: u64 = 0;
            let table_len = table.len() as u64 / 2;
            while !stop.load(Ordering::Acquire) {
                let mut dropped = 0;
                if !unpaced {
                    // A block is available once its last sample has "arrived".
                    let due_at = t0 + Duration::from_secs_f64((next_sample + BLOCK as u64) as f64 / FS);
                    let now = Instant::now();
                    if now < due_at {
                        thread::sleep(due_at - now);
                    }
                    let now = Instant::now();
                    let lag = now.saturating_duration_since(due_at).as_secs_f64();
                    rep.max_lag_ms = rep.max_lag_ms.max(lag * 1e3);
                    if lag > 1e-3 {
                        rep.late_blocks_1ms += 1;
                    }
                    // HackRF FIFO model: backlog beyond FIFO_CAP is lost.
                    let arrived = (now.duration_since(t0).as_secs_f64() * FS) as u64;
                    let backlog = arrived.saturating_sub(next_sample + BLOCK as u64);
                    if backlog > FIFO_CAP {
                        dropped = backlog - FIFO_CAP;
                        rep.dropped_samples += dropped;
                        rep.drop_events += 1;
                        rep.drop_log.push((now.duration_since(t0).as_secs_f64(), dropped, lag * 1e3));
                        next_sample += dropped;
                    }
                }
                let first = next_sample;
                w.publish(|b| {
                    b.first_sample = first;
                    b.n_samples = BLOCK;
                    b.dropped_before = dropped;
                    b.data.clear();
                    let mut pos = (first % table_len) as usize;
                    let mut left = BLOCK;
                    while left > 0 {
                        let n = left.min(table_len as usize - pos);
                        b.data.extend_from_slice(&table[2 * pos..2 * (pos + n)]);
                        pos = (pos + n) % table_len as usize;
                        left -= n;
                    }
                });
                next_sample += BLOCK as u64;
                rep.blocks += 1;
                rep.samples += BLOCK as u64;
            }
            rep.allocations = w.allocations;
            rep.cpu_s = dsp::thread_cpu_s() - cpu0;
            rep
        })
        .unwrap()
}

// ------------------------------------------------------ always-on (FFT) ---

#[derive(Default)]
struct FftReport {
    blocks: u64,
    samples: u64,
    lost_blocks: u64,
    gaps: u64,
    gap_samples: u64,
    frames: u64,
    peaks: Vec<(f64, f64)>,
    cpu_s: f64,
}

fn spawn_fft(mut r: Reader<i8>, stop: Arc<AtomicBool>) -> JoinHandle<FftReport> {
    thread::Builder::new()
        .name("fft".into())
        .spawn(move || {
            let cpu0 = dsp::thread_cpu_s();
            let mut rep = FftReport::default();
            let plan = FftPlanner::<f32>::new().plan_fft_forward(FFT_LEN);
            let mut scratch = vec![Complex32::default(); plan.get_inplace_scratch_len()];
            let mut buf = Vec::with_capacity(BLOCK);
            let mut acc = dsp::PowerAccum::new(FFT_LEN);
            let mut expected: Option<u64> = None;
            loop {
                match r.next(Duration::from_millis(20), &stop) {
                    Next::Block(b) => {
                        if let Some(e) = expected {
                            if b.first_sample != e {
                                rep.gaps += 1;
                                rep.gap_samples += b.first_sample.saturating_sub(e);
                            }
                        }
                        expected = Some(b.first_sample + b.n_samples as u64);
                        dsp::i8_to_c32(&b.data, &mut buf);
                        for f in buf.chunks_exact_mut(FFT_LEN) {
                            plan.process_with_scratch(f, &mut scratch);
                        }
                        acc.push(&buf);
                        rep.blocks += 1;
                        rep.samples += b.n_samples as u64;
                    }
                    Next::Lagged(n) => rep.lost_blocks += n,
                    Next::Idle => {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    Next::Closed => break,
                }
            }
            rep.frames = acc.frames;
            rep.peaks = acc.peaks(6, FS);
            rep.cpu_s = dsp::thread_cpu_s() - cpu0;
            rep
        })
        .unwrap()
}

// --------------------------------------------------------------- chains ---

struct ChainReport {
    spec: dsp::ChainSpec,
    start_cursor: u64,
    first_seq: Option<u64>,
    blocks: u64,
    lost_blocks: u64,
    gaps: u64,
    stats: dsp::DemodStats,
    cpu_s: f64,
}

struct ChainHandle {
    stop: Arc<AtomicBool>,
    join: JoinHandle<ChainReport>,
    first_out: mpsc::Receiver<Instant>,
    t_cmd: Instant,
    t_attached: Instant,
}

fn attach(ring: &Arc<Ring<i8>>, spec: dsp::ChainSpec, preroll_blocks: u64) -> ChainHandle {
    let t_cmd = Instant::now();
    let mut reader = if preroll_blocks > 0 {
        ring.reader_with_history(preroll_blocks)
    } else {
        ring.reader()
    };
    let start_cursor = reader.cursor();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let stop2 = stop.clone();
    let join = thread::Builder::new()
        .name(format!("chain-{}", spec.name))
        .spawn(move || {
            let cpu0 = dsp::thread_cpu_s();
            let (mut nodes, mut bufs) = build_chain(&spec);
            let mut input = Buf::C32(Vec::with_capacity(BLOCK));
            let mut rep = ChainReport {
                spec,
                start_cursor,
                first_seq: None,
                blocks: 0,
                lost_blocks: 0,
                gaps: 0,
                stats: Default::default(),
                cpu_s: 0.0,
            };
            let mut expected: Option<u64> = None;
            loop {
                match reader.next(Duration::from_millis(50), &stop2) {
                    Next::Block(b) => {
                        if let Some(e) = expected {
                            if b.first_sample != e {
                                rep.gaps += 1;
                            }
                        } else {
                            rep.first_seq = Some(b.seq);
                        }
                        expected = Some(b.first_sample + b.n_samples as u64);
                        let Buf::C32(v) = &mut input else { unreachable!() };
                        dsp::i8_to_c32(&b.data, v);
                        run_nodes(&mut nodes, &input, &mut bufs);
                        if rep.blocks == 0 {
                            let _ = tx.send(Instant::now());
                        }
                        rep.blocks += 1;
                    }
                    Next::Lagged(n) => rep.lost_blocks += n,
                    Next::Idle => {
                        if stop2.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    Next::Closed => break,
                }
            }
            rep.stats = nodes.iter().find_map(|n| n.stats()).unwrap_or_default();
            rep.cpu_s = dsp::thread_cpu_s() - cpu0;
            rep
        })
        .unwrap();
    ChainHandle { stop, join, first_out: rx, t_cmd, t_attached: Instant::now() }
}

fn detach(ring: &Ring<i8>, h: ChainHandle) -> (ChainReport, f64, Option<f64>, f64) {
    let t_first = h.first_out.try_recv().ok().map(|t| (t - h.t_cmd).as_secs_f64());
    let attach_s = (h.t_attached - h.t_cmd).as_secs_f64();
    let t0 = Instant::now();
    h.stop.store(true, Ordering::Release);
    ring.wake_all();
    let rep = h.join.join().unwrap();
    (rep, attach_s, t_first, t0.elapsed().as_secs_f64())
}

// ----------------------------------------------------------------- main ---

struct Args {
    cycles: usize,
    dwell_ms: u64,
    parallel: usize,
    preroll: u64,
    unpaced: bool,
    ring_blocks: usize,
    bench: bool,
    qos: bool,
}

fn args() -> Args {
    let mut a = Args {
        cycles: 200,
        dwell_ms: 50,
        parallel: 1,
        preroll: 0,
        unpaced: false,
        ring_blocks: 256,
        bench: false,
        qos: false,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < v.len() {
        let next = |i: usize| v.get(i + 1).expect("missing value").parse::<u64>().expect("number");
        match v[i].as_str() {
            "--cycles" => { a.cycles = next(i) as usize; i += 1 }
            "--dwell-ms" => { a.dwell_ms = next(i); i += 1 }
            "--parallel" => { a.parallel = next(i) as usize; i += 1 }
            "--preroll" => { a.preroll = next(i); i += 1 }
            "--ring-blocks" => { a.ring_blocks = next(i) as usize; i += 1 }
            "--unpaced" => a.unpaced = true,
            "--bench" => a.bench = true,
            "--qos" => a.qos = true,
            o => panic!("unknown arg {o}"),
        }
        i += 1;
    }
    a
}

fn main() {
    let a = args();
    let block_period = BLOCK as f64 / FS;
    eprintln!("generating synthetic table ({} samples)...", synth::LOOP_LEN);
    let table = Arc::new(synth::table_i8());
    if a.bench {
        bench(&table);
        return;
    }

    let ring = Ring::<i8>::new(a.ring_blocks);
    let stop_src = Arc::new(AtomicBool::new(false));
    let stop_fft = Arc::new(AtomicBool::new(false));
    let fft = spawn_fft(ring.reader(), stop_fft.clone());
    let src = spawn_source(ring.writer(), table.clone(), stop_src.clone(), a.unpaced, a.qos);

    thread::sleep(Duration::from_millis(500)); // warm-up
    let wall0 = Instant::now();
    let pcpu0 = dsp::process_cpu_s();
    let head0 = ring.head();

    let specs = dsp::chain_specs();
    let mut attach_lat = Vec::new();
    let mut first_lat = Vec::new();
    let mut detach_lat = Vec::new();
    let mut chain_bad = 0u64;
    let mut chain_lost = 0u64;
    let mut chain_gaps = 0u64;
    let mut chain_cpu = 0.0;
    let mut rms_err: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    let mut mean_hz: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    let attaches = AtomicU64::new(0);
    let reports = Mutex::new(Vec::new());

    for cycle in 0..a.cycles {
        let handles: Vec<_> = (0..a.parallel)
            .map(|p| {
                attaches.fetch_add(1, Ordering::Relaxed);
                attach(&ring, specs[(cycle + p) % 2], a.preroll)
            })
            .collect();
        thread::sleep(Duration::from_millis(a.dwell_ms));
        for h in handles {
            let (rep, att, first, det) = detach(&ring, h);
            attach_lat.push(att);
            if let Some(f) = first {
                first_lat.push(f);
            }
            detach_lat.push(det);
            chain_lost += rep.lost_blocks;
            chain_gaps += rep.gaps;
            chain_cpu += rep.cpu_s;
            // the chain's first block must be exactly the block its cursor was
            // bound to at attach time (nothing skipped between command and run)
            let aligned = rep.first_seq == Some(rep.start_cursor);
            if rep.blocks == 0 || !aligned {
                chain_bad += 1;
            }
            let k = if rep.spec.decim == 400 { 0 } else { 1 };
            rms_err[k].push(rep.stats.rms_ac() / rep.spec.expect_rms_hz - 1.0);
            mean_hz[k].push(rep.stats.mean());
            reports.lock().unwrap().push((rep.start_cursor, rep.blocks));
        }
    }

    thread::sleep(Duration::from_millis(200));
    let wall = wall0.elapsed().as_secs_f64();
    let pcpu = dsp::process_cpu_s() - pcpu0;
    let head1 = ring.head();
    stop_src.store(true, Ordering::Release);
    let src_rep = src.join().unwrap();
    // let the FFT reader drain everything published, then stop it
    thread::sleep(Duration::from_millis(100));
    stop_fft.store(true, Ordering::Release);
    ring.close();
    let fft_rep = fft.join().unwrap();

    let (a50, a99, amax) = dsp::pctl(&attach_lat);
    let (f50, f99, fmax) = dsp::pctl(&first_lat);
    let (d50, d99, dmax) = dsp::pctl(&detach_lat);
    let ms = |x: f64| x * 1e3;
    let consumed_ok = fft_rep.samples == src_rep.samples;
    let input_drops = src_rep.dropped_samples + fft_rep.gap_samples + fft_rep.lost_blocks * BLOCK as u64;
    let pass = input_drops == 0
        && consumed_ok
        && amax < block_period
        && chain_bad == 0
        && chain_lost == 0
        && chain_gaps == 0
        && attaches.load(Ordering::Relaxed) >= 100;

    println!("=== S1 owned mini-dataflow ===");
    println!("mode                     : {}{}", if a.unpaced { "UNPACED" } else { "paced 20 Msps" }, if a.qos { ", source thread QoS user-interactive" } else { ", default thread QoS" });
    println!("block / buffer period    : {} samples / {:.3} ms", BLOCK, ms(block_period));
    println!("ring                     : {} blocks ({:.2} s history)", a.ring_blocks, a.ring_blocks as f64 * block_period);
    println!("cycles x parallel        : {} x {} = {} attach/detach", a.cycles, a.parallel, attaches.load(Ordering::Relaxed));
    println!("dwell per chain          : {} ms, preroll {} blocks", a.dwell_ms, a.preroll);
    println!("measured window          : {:.2} s, {} blocks published", wall, head1 - head0);
    println!("source samples published : {}", src_rep.samples);
    let warm = 0.5; // s of warm-up before the measured window
    let window_drops: u64 = src_rep.drop_log.iter().filter(|d| d.0 >= warm).map(|d| d.1).sum();
    println!("source dropped in window : {} samples", window_drops);
    println!("source dropped total     : {} samples, {} events, log (t s, samples, lag ms) {:?}", src_rep.dropped_samples, src_rep.drop_events, src_rep.drop_log);
    println!("source late blocks >1ms  : {}, max lag {:.2} ms", src_rep.late_blocks_1ms, src_rep.max_lag_ms);
    println!("source block allocations : {} (steady state reuses blocks)", src_rep.allocations);
    println!("always-on consumed       : {} samples ({} blocks, {} FFT frames)", fft_rep.samples, fft_rep.blocks, fft_rep.frames);
    println!("always-on lost blocks    : {}", fft_rep.lost_blocks);
    println!("always-on counter gaps   : {} ({} samples)", fft_rep.gaps, fft_rep.gap_samples);
    println!("consumed == published    : {}", consumed_ok);
    println!("FFT peaks (Hz, dB)       : {:?}", fft_rep.peaks.iter().map(|(f, d)| (f.round(), (d * 10.0).round() / 10.0)).collect::<Vec<_>>());
    println!("attach latency ms        : p50 {:.4} p99 {:.4} max {:.4}", ms(a50), ms(a99), ms(amax));
    println!("attach->first output ms  : p50 {:.3} p99 {:.3} max {:.3}", ms(f50), ms(f99), ms(fmax));
    println!("detach latency ms        : p50 {:.4} p99 {:.4} max {:.4}", ms(d50), ms(d99), ms(dmax));
    println!("chains bad/lost/gaps     : {} / {} / {}", chain_bad, chain_lost, chain_gaps);
    for (k, s) in specs.iter().enumerate() {
        let (e50, _, emax) = dsp::pctl(&rms_err[k].iter().map(|x| x.abs()).collect::<Vec<_>>());
        let (m50, _, mmax) = dsp::pctl(&mean_hz[k].iter().map(|x| x.abs()).collect::<Vec<_>>());
        println!("demod {:22}: |rms/expected-1| p50 {:.4} max {:.4}; |mean| p50 {:.1} Hz max {:.1} Hz (n={})", s.name, e50, emax, m50, mmax, rms_err[k].len());
    }
    println!("process CPU              : {:.1} % of one core ({:.2} cores)", 100.0 * pcpu / wall, pcpu / wall);
    println!("source thread CPU        : {:.1} %", 100.0 * src_rep.cpu_s / (wall + 0.8));
    println!("fft thread CPU           : {:.1} %", 100.0 * fft_rep.cpu_s / (wall + 0.8));
    println!("chain threads CPU        : {:.1} % (avg while attached: {:.1} %)", 100.0 * chain_cpu / wall, 100.0 * chain_cpu / (attach_lat.len() as f64 * a.dwell_ms as f64 * 1e-3));
    println!("restart/recompile needed : no");
    println!("RESULT                   : {}", if pass { "PASS" } else { "FAIL" });
}

/// Single-thread throughput of each stage over the synthetic table.
fn bench(table: &Arc<Vec<i8>>) {
    let blocks: Vec<&[i8]> = table.chunks_exact(2 * BLOCK).collect();
    let dur = Duration::from_secs(2);
    let run = |name: &str, f: &mut dyn FnMut(&[i8])| {
        let t0 = Instant::now();
        let mut n = 0u64;
        let mut i = 0;
        while t0.elapsed() < dur {
            f(blocks[i % blocks.len()]);
            i += 1;
            n += BLOCK as u64;
        }
        let msps = n as f64 / t0.elapsed().as_secs_f64() / 1e6;
        println!("bench {:28}: {:8.1} Msps  ({:.1}x real time @20 Msps)", name, msps, msps / 20.0);
    };
    // ring publish (memcpy of one block) with one reader holding nothing
    let ring = Ring::<i8>::new(256);
    let mut w = ring.writer();
    run("ring publish (memcpy)", &mut |blk| {
        w.publish(|b| {
            b.data.clear();
            b.data.extend_from_slice(blk);
        });
    });
    let plan = FftPlanner::<f32>::new().plan_fft_forward(FFT_LEN);
    let mut scratch = vec![Complex32::default(); plan.get_inplace_scratch_len()];
    let mut buf = Vec::with_capacity(BLOCK);
    let mut acc = dsp::PowerAccum::new(FFT_LEN);
    run("always-on: conv+FFT4096+PSD", &mut |blk| {
        dsp::i8_to_c32(blk, &mut buf);
        for f in buf.chunks_exact_mut(FFT_LEN) {
            plan.process_with_scratch(f, &mut scratch);
        }
        acc.push(&buf);
    });
    for spec in dsp::chain_specs() {
        let (mut nodes, mut bufs) = build_chain(&spec);
        let mut input = Buf::C32(Vec::with_capacity(BLOCK));
        run(&format!("chain {}", spec.name), &mut |blk| {
            let Buf::C32(v) = &mut input else { unreachable!() };
            dsp::i8_to_c32(blk, v);
            run_nodes(&mut nodes, &input, &mut bufs);
        });
    }
}
