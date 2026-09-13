//! Spike S1 (FutureSDR 0.8.0): live attach/detach of DDC+FM chains on a
//! running 20 Msps capture.
//!
//! FutureSDR 0.8 has no API to add/remove blocks or edges on a *running*
//! flowgraph (RunningFlowgraph/FlowgraphHandle expose only message post/call,
//! describe and stop). What it does support is starting any number of
//! flowgraphs at runtime on one Runtime/scheduler. So the substrate here is:
//!
//!   Flowgraph A (always on):
//!     PacedSource -> Tap -> Fft(4096) -> PowerSink
//!                    Tap has message inputs `subscribe` / `unsubscribe`:
//!                    each subscriber is an mpsc sender; Tap copies every
//!                    chunk it forwards into each subscriber (try_send, never
//!                    blocks the always-on path; full queue = counted drop).
//!
//!   Flowgraph B_i (one per attached chain, started/stopped at runtime):
//!     ChannelSource -> XlatingFir -> Apply(FM discriminator) -> StatsSink
//!
//! Attach = build B_i, rt.start(B_i), call Tap.subscribe(Pmt::Any(sender)).
//! Detach = call Tap.unsubscribe(id) (drops the sender), B_i drains and
//! terminates, wait().

#[path = "../../common/dsp.rs"]
mod dsp;
#[path = "../../common/synth.rs"]
mod synth;

use futuresdr::blocks::{Apply, ChannelSource, Fft, FftDirection, XlatingFir};
use futuresdr::runtime::dev::prelude::*;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use synth::{BLOCK, FIFO_CAP, FS, LOOP_LEN};

const FFT_LEN: usize = 4096;
const CHAN_QUEUE: usize = 64;

// ------------------------------------------------------------ source ---

#[derive(Default)]
struct SrcStats {
    produced: AtomicU64,
    dropped: AtomicU64,
    drop_events: AtomicU64,
    late_blocks_1ms: AtomicU64,
    max_lag_us: AtomicU64,
    /// (t s, dropped samples, lag ms, free output space in blocks at the
    /// previous work() call: 0 = backpressure, >0 = source not scheduled)
    drop_log: Mutex<Vec<(f64, u64, f64, u64)>>,
}

#[derive(Block)]
struct PacedSource {
    #[output]
    output: DefaultCpuWriter<Complex32>,
    table: Arc<Vec<i8>>,
    unpaced: bool,
    t0: Instant,
    next_sample: u64,
    stats: Arc<SrcStats>,
    stop: Arc<AtomicBool>,
    timer: Option<Timer>,
    last_space_blocks: u64,
}

impl PacedSource {
    fn new(table: Arc<Vec<i8>>, unpaced: bool, stats: Arc<SrcStats>, stop: Arc<AtomicBool>) -> Self {
        let mut output = DefaultCpuWriter::<Complex32>::default();
        output.set_min_buffer_size_in_items(2 * BLOCK);
        Self {
            output,
            table,
            unpaced,
            t0: Instant::now(),
            next_sample: 0,
            stats,
            stop,
            timer: None,
            last_space_blocks: u64::MAX,
        }
    }
}

impl Kernel for PacedSource {
    type BlockOn = Timer;

    fn block_on(&mut self) -> Option<Pin<&mut Timer>> {
        self.timer.as_mut().map(Pin::new)
    }

    async fn init(&mut self, _mo: &mut MessageOutputs, _meta: &BlockMeta) -> Result<()> {
        self.t0 = Instant::now();
        Ok(())
    }

    async fn work(&mut self, io: &mut WorkIo, _mo: &mut MessageOutputs, _meta: &BlockMeta) -> Result<()> {
        self.timer = None;
        if self.stop.load(Ordering::Acquire) {
            io.finished = true;
            return Ok(());
        }
        let now = Instant::now();
        let pending_blocks = if self.unpaced {
            u64::MAX
        } else {
            let arrived = (now.duration_since(self.t0).as_secs_f64() * FS) as u64;
            let mut pending = arrived.saturating_sub(self.next_sample);
            if pending >= BLOCK as u64 {
                let lag_s = (pending - BLOCK as u64) as f64 / FS;
                self.stats.max_lag_us.fetch_max((lag_s * 1e6) as u64, Ordering::Relaxed);
            }
            // HackRF FIFO model (identical to the owned implementation).
            if pending > FIFO_CAP + BLOCK as u64 {
                let dropped = pending - FIFO_CAP - BLOCK as u64;
                self.stats.dropped.fetch_add(dropped, Ordering::Relaxed);
                self.stats.drop_events.fetch_add(1, Ordering::Relaxed);
                self.stats.drop_log.lock().unwrap().push((
                    now.duration_since(self.t0).as_secs_f64(),
                    dropped,
                    (pending - BLOCK as u64) as f64 / FS * 1e3,
                    self.last_space_blocks,
                ));
                self.next_sample += dropped;
                pending -= dropped;
            }
            pending / BLOCK as u64
        };

        let out = self.output.slice();
        let space_blocks = (out.len() / BLOCK) as u64;
        self.last_space_blocks = space_blocks;
        let n_blocks = pending_blocks.min(space_blocks) as usize;
        let n = n_blocks * BLOCK;
        if n > 0 {
            if !self.unpaced {
                // lateness of each block delivered now
                let arrived = (now.duration_since(self.t0).as_secs_f64() * FS) as u64;
                for k in 0..n_blocks as u64 {
                    let last = self.next_sample + (k + 1) * BLOCK as u64;
                    if arrived.saturating_sub(last) as f64 / FS > 1e-3 {
                        self.stats.late_blocks_1ms.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            let mut pos = (self.next_sample % LOOP_LEN as u64) as usize;
            let mut o = 0;
            while o < n {
                let k = (n - o).min(LOOP_LEN - pos);
                let src = &self.table[2 * pos..2 * (pos + k)];
                for (dst, p) in out[o..o + k].iter_mut().zip(src.chunks_exact(2)) {
                    *dst = Complex32::new(p[0] as f32 / 128.0, p[1] as f32 / 128.0);
                }
                o += k;
                pos = (pos + k) % LOOP_LEN;
            }
            self.output.produce(n);
            self.next_sample += n as u64;
            self.stats.produced.fetch_add(n as u64, Ordering::Relaxed);
        }

        if self.unpaced {
            if n > 0 {
                io.call_again = true;
            }
        } else {
            let next_due = self.t0 + Duration::from_secs_f64((self.next_sample + BLOCK as u64) as f64 / FS);
            let wait = next_due.saturating_duration_since(Instant::now());
            if wait.is_zero() && space_blocks > n_blocks as u64 {
                io.call_again = true;
            } else {
                self.timer = Some(Timer::after(wait.max(Duration::from_micros(500))));
            }
        }
        Ok(())
    }
}

// --------------------------------------------------------------- tap ---

#[derive(Default)]
struct SubCounters {
    sent: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Clone)]
struct Subscribe {
    id: u64,
    tx: mpsc::Sender<Box<[Complex32]>>,
    counters: Arc<SubCounters>,
}

#[derive(Block)]
#[message_inputs(subscribe, unsubscribe)]
struct Tap {
    #[input]
    input: DefaultCpuReader<Complex32>,
    #[output]
    output: DefaultCpuWriter<Complex32>,
    subs: Vec<Subscribe>,
    passed: Arc<AtomicU64>,
}

impl Tap {
    fn new(passed: Arc<AtomicU64>) -> Self {
        let mut output = DefaultCpuWriter::<Complex32>::default();
        output.set_min_buffer_size_in_items(2 * BLOCK);
        Self { input: Default::default(), output, subs: Vec::new(), passed }
    }

    async fn subscribe(&mut self, _io: &mut WorkIo, _mo: &mut MessageOutputs, _meta: &BlockMeta, p: Pmt) -> Result<Pmt> {
        if let Pmt::Any(a) = p {
            if let Ok(s) = a.to_any().downcast::<Subscribe>() {
                self.subs.push(*s);
                return Ok(Pmt::Ok);
            }
        }
        Ok(Pmt::InvalidValue)
    }

    async fn unsubscribe(&mut self, _io: &mut WorkIo, _mo: &mut MessageOutputs, _meta: &BlockMeta, p: Pmt) -> Result<Pmt> {
        if let Pmt::U64(id) = p {
            self.subs.retain(|s| s.id != id);
            return Ok(Pmt::Ok);
        }
        Ok(Pmt::InvalidValue)
    }
}

impl Kernel for Tap {
    async fn work(&mut self, io: &mut WorkIo, _mo: &mut MessageOutputs, _meta: &BlockMeta) -> Result<()> {
        let i = self.input.slice();
        let i_len = i.len();
        let o = self.output.slice();
        let m = i_len.min(o.len());
        if m > 0 {
            o[..m].copy_from_slice(&i[..m]);
            self.subs.retain(|s| match s.tx.try_send(i[..m].to_vec().into_boxed_slice()) {
                Ok(()) => {
                    s.counters.sent.fetch_add(m as u64, Ordering::Relaxed);
                    true
                }
                Err(mpsc::TrySendError::Full(_)) => {
                    s.counters.dropped.fetch_add(m as u64, Ordering::Relaxed);
                    true
                }
                Err(mpsc::TrySendError::Disconnected(_)) => false,
            });
            self.input.consume(m);
            self.output.produce(m);
            self.passed.fetch_add(m as u64, Ordering::Relaxed);
        }
        if self.input.finished() && m == i_len {
            self.subs.clear();
            io.finished = true;
        }
        Ok(())
    }
}

// ------------------------------------------------------------- sinks ---

#[derive(Block)]
struct PowerSink {
    #[input]
    input: DefaultCpuReader<Complex32>,
    acc: dsp::PowerAccum,
    consumed: Arc<AtomicU64>,
    peaks: Arc<Mutex<Vec<(f64, f64)>>>,
}

impl Kernel for PowerSink {
    async fn work(&mut self, io: &mut WorkIo, _mo: &mut MessageOutputs, _meta: &BlockMeta) -> Result<()> {
        let i = self.input.slice();
        let n = i.len() / FFT_LEN * FFT_LEN;
        let rest = i.len() - n;
        if n > 0 {
            self.acc.push(&i[..n]);
            self.input.consume(n);
            self.consumed.fetch_add(n as u64, Ordering::Relaxed);
        }
        if self.input.finished() && rest < FFT_LEN {
            *self.peaks.lock().unwrap() = self.acc.peaks(6, FS);
            io.finished = true;
        }
        Ok(())
    }
}

#[derive(Default)]
struct ChainShared {
    stats: dsp::DemodStats,
    first_out: Option<Instant>,
}

#[derive(Block)]
struct StatsSink {
    #[input]
    input: DefaultCpuReader<f32>,
    shared: Arc<Mutex<ChainShared>>,
}

impl Kernel for StatsSink {
    async fn work(&mut self, io: &mut WorkIo, _mo: &mut MessageOutputs, _meta: &BlockMeta) -> Result<()> {
        let i = self.input.slice();
        let n = i.len();
        if n > 0 {
            let mut s = self.shared.lock().unwrap();
            if s.first_out.is_none() {
                s.first_out = Some(Instant::now());
            }
            s.stats.push(i);
            drop(s);
            self.input.consume(n);
        }
        if self.input.finished() {
            io.finished = true;
        }
        Ok(())
    }
}

// ------------------------------------------------------------ chains ---

fn chain_flowgraph(
    spec: &dsp::ChainSpec,
    rx: mpsc::Receiver<Box<[Complex32]>>,
) -> anyhow::Result<(Flowgraph, Arc<Mutex<ChainShared>>)> {
    let mut fg = Flowgraph::new();
    let shared = Arc::new(Mutex::new(ChainShared::default()));
    let src = ChannelSource::<Complex32>::new(rx);
    let ddc = XlatingFir::with_taps(dsp::lowpass_taps(spec.decim), spec.decim, spec.offset_hz as f32, FS as f32);
    let mut fm = dsp::FmDiscriminator::new(FS / spec.decim as f64);
    let demod = Apply::new(move |x: &Complex32| -> f32 { fm.step(*x) });
    let snk = StatsSink { input: Default::default(), shared: shared.clone() };
    connect!(fg, src > ddc > demod > snk);
    Ok((fg, shared))
}

struct Attached {
    id: u64,
    spec: dsp::ChainSpec,
    running: RunningFlowgraph,
    shared: Arc<Mutex<ChainShared>>,
    counters: Arc<SubCounters>,
    t_cmd: Instant,
    attach_s: f64,
}

// -------------------------------------------------------------- main ---

struct Args {
    cycles: usize,
    dwell_ms: u64,
    parallel: usize,
    unpaced: bool,
    run_ms: u64,
    bench: bool,
    chain_buf_kib: u64,
}

fn args() -> Args {
    let mut a = Args { cycles: 200, dwell_ms: 50, parallel: 1, unpaced: false, run_ms: 0, bench: false, chain_buf_kib: 0 };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < v.len() {
        let next = |i: usize| v.get(i + 1).expect("missing value").parse::<u64>().expect("number");
        match v[i].as_str() {
            "--cycles" => { a.cycles = next(i) as usize; i += 1 }
            "--dwell-ms" => { a.dwell_ms = next(i); i += 1 }
            "--parallel" => { a.parallel = next(i) as usize; i += 1 }
            "--run-ms" => { a.run_ms = next(i); i += 1 }
            "--chain-buf-kib" => { a.chain_buf_kib = next(i); i += 1 }
            "--unpaced" => a.unpaced = true,
            "--bench" => a.bench = true,
            o => panic!("unknown arg {o}"),
        }
        i += 1;
    }
    a
}

fn main() -> anyhow::Result<()> {
    let a = args();
    // two blocks of Complex32 per stream edge
    futuresdr::runtime::config::set("buffer_size", (2 * BLOCK * 16) as u64);
    futuresdr::runtime::config::set("log_level", "warn");
    let block_period = BLOCK as f64 / FS;
    eprintln!("generating synthetic table ({} samples)...", LOOP_LEN);
    let table = Arc::new(synth::table_i8());
    let rt = Runtime::new();
    if a.bench {
        return bench(&rt, &table);
    }

    // ---- flowgraph A (always on)
    let src_stats = Arc::new(SrcStats::default());
    let stop = Arc::new(AtomicBool::new(false));
    let passed = Arc::new(AtomicU64::new(0));
    let consumed = Arc::new(AtomicU64::new(0));
    let peaks = Arc::new(Mutex::new(Vec::new()));
    let mut fg = Flowgraph::new();
    let src = PacedSource::new(table.clone(), a.unpaced, src_stats.clone(), stop.clone());
    let tap = Tap::new(passed.clone());
    let fft = Fft::with_options(FFT_LEN, FftDirection::Forward, false, None);
    let snk = PowerSink {
        input: Default::default(),
        acc: dsp::PowerAccum::new(FFT_LEN),
        consumed: consumed.clone(),
        peaks: peaks.clone(),
    };
    connect!(fg, src > tap > fft > snk);
    let tap_id: BlockId = tap.into();
    let running_a = rt.start(fg)?;

    std::thread::sleep(Duration::from_millis(500));
    let wall0 = Instant::now();
    let pcpu0 = dsp::process_cpu_s();
    let produced0 = src_stats.produced.load(Ordering::Relaxed);
    let consumed0 = consumed.load(Ordering::Relaxed);
    let drops0 = src_stats.dropped.load(Ordering::Relaxed);

    let specs = dsp::chain_specs();
    let mut attach_lat = Vec::new();
    let mut first_lat = Vec::new();
    let mut detach_lat = Vec::new();
    let mut chain_dropped = 0u64;
    let mut chain_bad = 0u64;
    let mut rms_err: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    let mut mean_hz: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    let mut next_id = 1u64;
    let mut n_attach = 0u64;
    let (mut lat_build, mut lat_start, mut lat_sub) = (Vec::new(), Vec::new(), Vec::new());
    if a.chain_buf_kib > 0 {
        // stream-edge buffer size used for chain flowgraphs created from now on
        futuresdr::runtime::config::set("buffer_size", a.chain_buf_kib * 1024);
    }

    for cycle in 0..a.cycles {
        let mut attached = Vec::new();
        for p in 0..a.parallel {
            let spec = specs[(cycle + p) % 2];
            let t_cmd = Instant::now();
            let (tx, rx) = mpsc::channel(CHAN_QUEUE);
            let (fgb, shared) = chain_flowgraph(&spec, rx)?;
            let t_built = Instant::now();
            let running = rt.start(fgb)?;
            let t_started = Instant::now();
            let counters = Arc::new(SubCounters::default());
            let id = next_id;
            next_id += 1;
            let msg = Subscribe { id, tx, counters: counters.clone() };
            let r = block_on(running_a.block(tap_id).call("subscribe", Pmt::Any(Box::new(msg))))?;
            assert!(matches!(r, Pmt::Ok), "subscribe failed: {r:?}");
            let attach_s = t_cmd.elapsed().as_secs_f64();
            lat_build.push((t_built - t_cmd).as_secs_f64());
            lat_start.push((t_started - t_built).as_secs_f64());
            lat_sub.push(t_started.elapsed().as_secs_f64());
            n_attach += 1;
            attached.push(Attached { id, spec, running, shared, counters, t_cmd, attach_s });
        }
        std::thread::sleep(Duration::from_millis(a.dwell_ms));
        for c in attached {
            let t0 = Instant::now();
            block_on(running_a.block(tap_id).call("unsubscribe", Pmt::U64(c.id)))?;
            c.running.wait()?;
            detach_lat.push(t0.elapsed().as_secs_f64());
            attach_lat.push(c.attach_s);
            let s = c.shared.lock().unwrap();
            if let Some(f) = s.first_out {
                first_lat.push((f - c.t_cmd).as_secs_f64());
            }
            let sent = c.counters.sent.load(Ordering::Relaxed);
            let dropped = c.counters.dropped.load(Ordering::Relaxed);
            chain_dropped += dropped;
            // all samples sent to the chain came out of the decimator
            let expect_out = sent as f64 / c.spec.decim as f64;
            let ntaps = (4 * c.spec.decim + 1) as f64 / c.spec.decim as f64;
            if s.stats.n == 0 || (s.stats.n as f64 - expect_out).abs() > ntaps + 2.0 {
                chain_bad += 1;
            }
            let k = if c.spec.decim == 400 { 0 } else { 1 };
            rms_err[k].push(s.stats.rms_ac() / c.spec.expect_rms_hz - 1.0);
            mean_hz[k].push(s.stats.mean());
        }
    }
    if a.run_ms > 0 {
        std::thread::sleep(Duration::from_millis(a.run_ms));
    }

    std::thread::sleep(Duration::from_millis(200));
    let wall = wall0.elapsed().as_secs_f64();
    let pcpu = dsp::process_cpu_s() - pcpu0;
    let window_produced = src_stats.produced.load(Ordering::Relaxed) - produced0;
    let window_consumed = consumed.load(Ordering::Relaxed) - consumed0;
    let window_drops = src_stats.dropped.load(Ordering::Relaxed) - drops0;
    stop.store(true, Ordering::Release);
    running_a.wait()?;

    let produced = src_stats.produced.load(Ordering::Relaxed);
    let consumed_all = consumed.load(Ordering::Relaxed);
    let total_drops = src_stats.dropped.load(Ordering::Relaxed);
    let (a50, a99, amax) = dsp::pctl(&attach_lat);
    let (f50, f99, fmax) = dsp::pctl(&first_lat);
    let (d50, d99, dmax) = dsp::pctl(&detach_lat);
    let ms = |x: f64| x * 1e3;
    let pass = window_drops == 0
        && produced == consumed_all
        && passed.load(Ordering::Relaxed) == produced
        && amax < block_period
        && chain_dropped == 0
        && chain_bad == 0
        && n_attach >= 100;

    println!("=== S1 FutureSDR 0.8.0 ===");
    println!("mode                     : {}", if a.unpaced { "UNPACED" } else { "paced 20 Msps" });
    println!("block / buffer period    : {} samples / {:.3} ms", BLOCK, ms(block_period));
    println!("cycles x parallel        : {} x {} = {} attach/detach", a.cycles, a.parallel, n_attach);
    println!("dwell per chain          : {} ms", a.dwell_ms);
    println!("measured window          : {:.2} s", wall);
    println!("window samples produced  : {} ({:.2} Msps)", window_produced, window_produced as f64 / wall / 1e6);
    println!("window samples consumed  : {} ({:.2} Msps)", window_consumed, window_consumed as f64 / wall / 1e6);
    println!("source dropped in window : {} samples", window_drops);
    println!("source dropped total     : {} samples, {} events, log {:?}", total_drops, src_stats.drop_events.load(Ordering::Relaxed), src_stats.drop_log.lock().unwrap());
    println!("source late blocks >1ms  : {}, max lag {:.2} ms", src_stats.late_blocks_1ms.load(Ordering::Relaxed), src_stats.max_lag_us.load(Ordering::Relaxed) as f64 / 1e3);
    println!("total produced/tap/sink  : {} / {} / {}", produced, passed.load(Ordering::Relaxed), consumed_all);
    println!("consumed == produced     : {}", produced == consumed_all);
    println!("FFT peaks (Hz, dB)       : {:?}", peaks.lock().unwrap().iter().map(|(f, d)| (f.round(), (d * 10.0).round() / 10.0)).collect::<Vec<_>>());
    println!("attach latency ms        : p50 {:.4} p99 {:.4} max {:.4}", ms(a50), ms(a99), ms(amax));
    for (name, v) in [("  build flowgraph", &lat_build), ("  rt.start(fg)", &lat_start), ("  Tap.subscribe call", &lat_sub)] {
        let (p50, p99, max) = dsp::pctl(v);
        println!("{:25}: p50 {:.4} p99 {:.4} max {:.4}", name, ms(p50), ms(p99), ms(max));
    }
    println!("chain edge buffer        : {}", if a.chain_buf_kib > 0 { format!("{} KiB", a.chain_buf_kib) } else { format!("{} KiB (same as always-on)", 2 * BLOCK * 16 / 1024) });
    println!("attach->first output ms  : p50 {:.3} p99 {:.3} max {:.3}", ms(f50), ms(f99), ms(fmax));
    println!("detach latency ms        : p50 {:.4} p99 {:.4} max {:.4}", ms(d50), ms(d99), ms(dmax));
    println!("chains bad / dropped     : {} / {} samples", chain_bad, chain_dropped);
    for (k, s) in specs.iter().enumerate() {
        let (e50, _, emax) = dsp::pctl(&rms_err[k].iter().map(|x| x.abs()).collect::<Vec<_>>());
        let (m50, _, mmax) = dsp::pctl(&mean_hz[k].iter().map(|x| x.abs()).collect::<Vec<_>>());
        println!("demod {:22}: |rms/expected-1| p50 {:.4} max {:.4}; |mean| p50 {:.1} Hz max {:.1} Hz (n={})", s.name, e50, emax, m50, mmax, rms_err[k].len());
    }
    println!("process CPU              : {:.1} % of one core ({:.2} cores)", 100.0 * pcpu / wall, pcpu / wall);
    println!("restart/recompile needed : no (new flowgraph per chain on the running Runtime)");
    println!("RESULT                   : {}", if pass { "PASS" } else { "FAIL" });
    Ok(())
}

/// Unpaced throughput of one chain flowgraph (ChannelSource fed as fast as it
/// accepts) — comparable to the owned `--bench` chain numbers.
fn bench(rt: &Runtime, table: &Arc<Vec<i8>>) -> anyhow::Result<()> {
    let n_blocks = 1024usize;
    for spec in dsp::chain_specs() {
        let blocks: Vec<Box<[Complex32]>> = table
            .chunks_exact(2 * BLOCK)
            .cycle()
            .take(n_blocks)
            .map(|b| {
                let mut v = Vec::new();
                dsp::i8_to_c32(b, &mut v);
                v.into_boxed_slice()
            })
            .collect();
        let (tx, rx) = mpsc::channel(4);
        let (fg, _shared) = chain_flowgraph(&spec, rx)?;
        let running = rt.start(fg)?;
        let t0 = Instant::now();
        let feeder = std::thread::spawn(move || {
            for b in blocks {
                block_on(tx.send(b)).unwrap();
            }
        });
        feeder.join().unwrap();
        running.wait()?;
        let msps = (n_blocks * BLOCK) as f64 / t0.elapsed().as_secs_f64() / 1e6;
        println!("bench chain {:22}: {:8.1} Msps ({:.1}x real time @20 Msps)", spec.name, msps, msps / 20.0);
    }
    Ok(())
}
