//! Blocks A (T-086 iq and symbol groups; T-610 adds the streaming `viterbi`, T-612
//! `mlevel_slicer`) allocate nothing in `process` over steady-state chunks, including chunks
//! flagged `DISCONTINUITY`/`RESET` (ADR-0011 §1.4 rule 1), and a restart leaves each block
//! exactly as a freshly built one (T-104).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use hk_blocks::{
    Block, BuildCtx, ChunkFlags, ChunkMeta, Input, Io, Output, PortInfo, PortSlice, PortVec,
    Registry, TapMask,
};
use hk_recipe::PortType;
use num_complex::Complex32;
use serde_json::json;

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static COUNT_HERE: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNT_HERE.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNT_HERE.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations made by `f` on this thread.
fn allocations(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.load(Ordering::Relaxed);
    COUNT_HERE.with(|c| c.set(true));
    f();
    COUNT_HERE.with(|c| c.set(false));
    ALLOCS.load(Ordering::Relaxed) - before
}

const CHUNK: usize = 4_096;
const WARMUP_CHUNKS: usize = 6;
const MEASURED_CHUNKS: usize = 14;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn sym(&mut self) -> f32 {
        if self.next() < 0.5 { -1.0 } else { 1.0 }
    }
}

/// A test signal of `n` items on a `ty` port at `fs`.
enum Signal {
    Iq(Vec<Complex32>),
    Real(Vec<f32>),
    Soft(Vec<f32>),
    Bits(Vec<u8>),
}

impl Signal {
    fn slice(&self, a: usize, b: usize) -> PortSlice<'_> {
        match self {
            Signal::Iq(v) => PortSlice::Iq(&v[a..b]),
            Signal::Real(v) => PortSlice::Real(&v[a..b]),
            Signal::Soft(v) => PortSlice::Soft(&v[a..b]),
            Signal::Bits(v) => PortSlice::Bits(&v[a..b]),
        }
    }

    fn ty(&self) -> PortType {
        match self {
            Signal::Iq(_) => PortType::Iq,
            Signal::Real(_) => PortType::Real,
            Signal::Soft(_) => PortType::Soft,
            Signal::Bits(_) => PortType::Bits,
        }
    }
}

fn items() -> usize {
    CHUNK * (WARMUP_CHUNKS + MEASURED_CHUNKS)
}

/// FM-modulated random symbols plus noise.
fn fsk_iq(fs: f64, rate: f64, dev: f64) -> Signal {
    let mut rng = Rng(1);
    let sps = (fs / rate).round().max(1.0) as usize;
    let mut ph = 0.0f64;
    let mut sym = 1.0;
    Signal::Iq(
        (0..items())
            .map(|i| {
                if i % sps == 0 {
                    sym = f64::from(rng.sym());
                }
                ph += std::f64::consts::TAU * dev * sym / fs;
                Complex32::new(
                    ph.cos() as f32 + 0.01 * (rng.next() as f32 - 0.5),
                    ph.sin() as f32 + 0.01 * (rng.next() as f32 - 0.5),
                )
            })
            .collect(),
    )
}

/// T-875: BPSK bursts (80 symbols at 4 800 Bd, 300 Hz off) every 3 000 samples at 48 kS/s over
/// a quiet floor, so `psk_demod`'s burst mode finds, acquires, tracks, closes and sometimes
/// defers a burst inside the measured chunks.
fn bpsk_bursts() -> Signal {
    let mut rng = Rng(9);
    let mut sym = 1.0f32;
    Signal::Iq(
        (0..items())
            .map(|i| {
                let t = i % 3_000;
                let on = (500..1_300).contains(&t);
                if t % 10 == 0 {
                    sym = rng.sym();
                }
                let ph = std::f64::consts::TAU * 300.0 * i as f64 / 48_000.0;
                let a = if on { sym } else { 0.0 };
                Complex32::new(
                    a * ph.cos() as f32 + 0.01 * (rng.next() as f32 - 0.5),
                    a * ph.sin() as f32 + 0.01 * (rng.next() as f32 - 0.5),
                )
            })
            .collect(),
    )
}

/// Mode S-like frames (preamble + 112 data bits) every 1 500 samples at 2 Msps, over noise.
fn ppm_iq() -> Signal {
    let mut rng = Rng(2);
    let mut x: Vec<Complex32> = (0..items())
        .map(|_| Complex32::new(0.02 * rng.next() as f32, 0.02 * rng.next() as f32))
        .collect();
    let mut start = 100;
    while start + 300 < x.len() {
        for k in [0, 2, 7, 9] {
            x[start + k] += Complex32::new(1.0, 0.0);
        }
        for j in 0..112 {
            let one = if j < 5 {
                [1, 0, 0, 0, 1][j] == 1
            } else {
                rng.sym() > 0.0
            };
            x[start + 16 + 2 * j + usize::from(!one)] += Complex32::new(1.0, 0.0);
        }
        start += 1_500;
    }
    Signal::Iq(x)
}

/// Random ±1 symbols of `sps` samples plus noise.
fn nrz(sps: usize, seed: u64) -> Vec<f32> {
    let mut rng = Rng(seed);
    let mut v = 1.0;
    (0..items())
        .map(|i| {
            if i % sps == 0 {
                v = rng.sym();
            }
            v + 0.1 * (rng.next() as f32 - 0.5)
        })
        .collect()
}

/// RDS-like MPX: 19 kHz pilot plus a 57 kHz subcarrier.
fn mpx(fs: f64) -> Signal {
    Signal::Real(
        (0..items())
            .map(|i| {
                let th = std::f64::consts::TAU * 19_000.0 * i as f64 / fs;
                (0.1 * th.cos() + 0.05 * (3.0 * th).sin()) as f32
            })
            .collect(),
    )
}

fn build(name: &str, params: serde_json::Value, ty: PortType) -> Box<dyn Block> {
    let maps = BTreeMap::new();
    let types = [ty];
    let ctx = BuildCtx {
        field_maps: &maps,
        input_types: &types,
    };
    Registry::builtin()
        .build(name, params.as_object().unwrap(), &ctx)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// A block with its pre-sized outputs.
struct Node {
    block: Box<dyn Block>,
    outputs: Vec<Output>,
}

impl Node {
    fn new(name: &str, params: &serde_json::Value, sig: &Signal, fs: f64) -> Self {
        let mut block = build(name, params.clone(), sig.ty());
        let info = PortInfo {
            ty: sig.ty(),
            rate_hz: fs,
            max_items: CHUNK,
            hold_items: 0,
        };
        let outs = block.init(&[info]).expect("init");
        let outputs = outs.iter().map(Output::for_port).collect();
        Self { block, outputs }
    }

    /// Processes chunk `k` of `sig` (every port tapped).
    fn feed(&mut self, sig: &Signal, fs: f64, k: usize, flags: ChunkFlags) {
        let meta = ChunkMeta {
            index: (k * CHUNK) as u64,
            source_index: (k * CHUNK) as f64,
            source_per_item: 1.0,
            rate_hz: fs,
            channel: 0,
            flags,
        };
        let inputs = [Input {
            meta,
            data: sig.slice(k * CHUNK, (k + 1) * CHUNK),
        }];
        for o in &mut self.outputs {
            o.begin_chunk();
        }
        self.block
            .process(&mut Io::new(&inputs, &mut self.outputs).with_taps(TapMask(u32::MAX)))
            .expect("process");
    }
}

/// Equal items; frames compare bytes and source index (their `index` is a running count on
/// the port, not state a restart clears).
fn same_items(a: &PortVec, b: &PortVec) -> bool {
    match (a, b) {
        (PortVec::Frames(x), PortVec::Frames(y)) => {
            x.len() == y.len()
                && x.iter().zip(y.iter()).all(|(f, g)| {
                    f.bytes == g.bytes
                        && f.info.bit_len == g.info.bit_len
                        && f.info.source_index == g.info.source_index
                })
        }
        _ => a == b,
    }
}

/// Measured chunks flagged as restarts.
fn flags_of(k: usize) -> ChunkFlags {
    match k {
        0 => ChunkFlags::DISCONTINUITY,
        k if k == WARMUP_CHUNKS + 3 => ChunkFlags::DISCONTINUITY,
        k if k == WARMUP_CHUNKS + 8 => ChunkFlags::RESET,
        _ => ChunkFlags::NONE,
    }
}

#[test]
fn blocks_a_process_allocates_nothing_in_steady_state_and_across_restarts() {
    let iq_fs = 240_000.0;
    let fsk = fsk_iq(48_000.0, 1_200.0, 2_400.0);
    let bursts = bpsk_bursts();
    let fm = fsk_iq(iq_fs, 1_187.5, 50_000.0);
    let ppm = ppm_iq();
    let mpx = mpx(iq_fs);
    let real = Signal::Real(nrz(8, 3));
    let biphase = Signal::Real(nrz(4, 4));
    let soft = Signal::Soft(nrz(1, 5));
    let bits = Signal::Bits(nrz(1, 6).iter().map(|&v| u8::from(v > 0.0)).collect());
    let adsb = json!({"bit_rate_bd": 1000000, "preamble": "0xA140", "preamble_chips": 16,
        "frame_bits": 112, "length_from": {"offset_bits": 0, "bits": 5,
        "cases": [{"min": 16, "max": 31, "frame_bits": 112}], "default_bits": 56}});
    let clock = |extra: serde_json::Value| {
        let mut p = json!({"symbol_rate_bd": 1200});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        p
    };
    let cases: Vec<(&str, serde_json::Value, &Signal, f64)> = vec![
        ("mix", json!({"offset_hz": 1000}), &fm, iq_fs),
        (
            "lowpass",
            json!({"cutoff_hz": 20000, "transition_hz": 5000}),
            &fm,
            iq_fs,
        ),
        ("lowpass", json!({"cutoff_hz": 2000}), &real, 9_600.0),
        ("resample", json!({"output_rate_hz": 48000}), &fm, iq_fs),
        ("resample", json!({"output_rate_hz": 2400}), &real, 9_600.0),
        ("fm_demod", json!({}), &fm, iq_fs),
        (
            "fm_demod",
            json!({"deviation_hz": 75000, "output_rate_hz": 48000, "deemphasis_s": 75e-6}),
            &fm,
            iq_fs,
        ),
        ("am_demod", json!({}), &fm, iq_fs),
        ("am_demod", json!({"mode": "envelope"}), &fm, iq_fs),
        (
            "fsk_demod",
            json!({"offset_tracking_s": 0.5}),
            &fsk,
            48_000.0,
        ),
        ("msk_demod", json!({"symbol_rate_bd": 1200}), &fsk, 48_000.0),
        ("ppm_demod", adsb, &ppm, 2e6),
        (
            "subcarrier",
            json!({"carrier_hz": 57000, "bandwidth_hz": 4800, "output_rate_hz": 9500,
                   "reference": {"pilot_hz": 19000, "multiple": 3}, "phase_tracking": "bpsk"}),
            &mpx,
            iq_fs,
        ),
        (
            "subcarrier",
            json!({"carrier_hz": 57000, "bandwidth_hz": 4800, "output_rate_hz": 9500,
                   "phase_tracking": "qpsk"}),
            &mpx,
            iq_fs,
        ),
        // T-873: the FM stereo decoder (audio group) runs on the same multiplex.
        ("stereo_decode", json!({}), &mpx, iq_fs),
        ("clock_recovery", clock(json!({})), &real, 9_600.0),
        (
            "clock_recovery",
            clock(json!({"algorithm": "mueller-muller", "pulse": "rrc", "loop_bandwidth": 0.25})),
            &real,
            9_600.0,
        ),
        (
            "clock_recovery",
            clock(json!({"pulse": "biphase"})),
            &biphase,
            4_800.0,
        ),
        (
            "clock_recovery",
            clock(json!({"pulse": "biphase", "algorithm": "max-contrast"})),
            &biphase,
            4_800.0,
        ),
        (
            "clock_recovery",
            clock(json!({"soft_from": "magnitude"})),
            &fsk,
            48_000.0,
        ),
        // T-609: the liquid path through the resampler (d8psk at 4 samples/symbol → 5), at an
        // exact integer rate (8psk: no resampler), and the native OQPSK path. The measured
        // chunks include the acquisition FFT (it lands inside the warm-up for these rates).
        (
            "psk_demod",
            json!({"modulation": "qpsk", "symbol_rate_bd": 4800}),
            &fsk,
            48_000.0,
        ),
        (
            "psk_demod",
            json!({"modulation": "d8psk", "symbol_rate_bd": 12000}),
            &fsk,
            48_000.0,
        ),
        (
            "psk_demod",
            json!({"modulation": "8psk", "symbol_rate_bd": 9600, "max_offset_hz": 0}),
            &fsk,
            48_000.0,
        ),
        (
            "psk_demod",
            json!({"modulation": "oqpsk", "symbol_rate_bd": 4800, "pulse": "half-sine"}),
            &fsk,
            48_000.0,
        ),
        // T-875: burst mode — detector, per-burst estimators (two FFTs), fractional-delay feed,
        // flush and the one-burst-per-chunk deferral — on bursts, and on the native path.
        (
            "psk_demod",
            json!({"modulation": "bpsk", "symbol_rate_bd": 4800, "burst": true}),
            &bursts,
            48_000.0,
        ),
        (
            "psk_demod",
            json!({"modulation": "oqpsk", "symbol_rate_bd": 4800, "burst": true}),
            &bursts,
            48_000.0,
        ),
        ("slicer", json!({}), &soft, 2_400.0),
        // T-612: the M-ary decision, auto (the windowed level fit) and fixed.
        ("mlevel_slicer", json!({"levels": 4}), &soft, 2_400.0),
        (
            "mlevel_slicer",
            json!({"levels": 8, "window": 1000, "mapping": "natural", "bit_order": "lsb"}),
            &soft,
            2_400.0,
        ),
        (
            "mlevel_slicer",
            json!({"levels": 4, "thresholds": "fixed", "fixed_levels": [-0.5, 0.0, 0.5],
                   "mapping": "table", "table": [3, 2, 0, 1]}),
            &soft,
            2_400.0,
        ),
        ("diff_decode", json!({}), &bits, 2_400.0),
        ("nrzi", json!({}), &bits, 2_400.0),
        ("manchester", json!({}), &soft, 2_400.0),
        ("manchester", json!({"convention": "ieee"}), &bits, 2_400.0),
        // T-610: the streaming Viterbi decoder at symbol rate, soft with the auto phase search
        // (two lanes), and hard on a punctured code (four lanes).
        (
            "viterbi",
            json!({"constraint_length": 7, "polys": ["0x4F", "0x6D"], "invert": [false, true]}),
            &soft,
            2_400.0,
        ),
        (
            "viterbi",
            json!({"constraint_length": 7, "polys": ["0x4F", "0x6D"], "puncture": ["101", "110"],
                   "traceback_bits": 128}),
            &bits,
            2_400.0,
        ),
    ];

    let mut failures = Vec::new();
    for (name, params, sig, fs) in &cases {
        let mut node = Node::new(name, params, sig, *fs);
        for k in 0..WARMUP_CHUNKS {
            node.feed(sig, *fs, k, flags_of(k));
        }
        let n = allocations(|| {
            for k in WARMUP_CHUNKS..WARMUP_CHUNKS + MEASURED_CHUNKS {
                node.feed(sig, *fs, k, flags_of(k));
            }
        });
        if n > 0 {
            failures.push(format!("{name} {params}: {n} allocations"));
        }

        // A restart leaves the block as a freshly built one.
        let restart = WARMUP_CHUNKS + MEASURED_CHUNKS - 1;
        node.feed(sig, *fs, restart, ChunkFlags::DISCONTINUITY);
        let mut fresh = Node::new(name, params, sig, *fs);
        fresh.feed(sig, *fs, restart, ChunkFlags::DISCONTINUITY);
        if !same_items(&node.outputs[0].data, &fresh.outputs[0].data) {
            failures.push(format!("{name} {params}: output after a restart differs"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
