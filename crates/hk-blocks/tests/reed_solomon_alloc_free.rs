//! T-611: `reed_solomon` allocates nothing in `process` in steady state (ADR-0011 §1.4 rule 1:
//! on `frames` per-frame allocation is allowed but bounded; this block needs none). The decoder
//! scratch is sized at build and the frame buffers grow to the longest frame once, so after a
//! warm-up chunk every later chunk — clean codewords, `t` errors, `t + 1` errors, short frames
//! — runs with zero allocations, across the CCSDS (interleaved, dual basis) and P25 (GF(64))
//! shapes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use hk_blocks::{
    BuildCtx, ChunkFlags, ChunkMeta, FrameBuf, FrameInfo, Input, Io, Output, PortInfo, PortSlice,
    PortVec, Registry,
};
use hk_model::CrcStatus;
use hk_recipe::PortType;
use serde_json::{Value, json};

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

fn allocations(f: impl FnOnce()) -> u64 {
    let before = ALLOCS.load(Ordering::Relaxed);
    COUNT_HERE.with(|c| c.set(true));
    f();
    COUNT_HERE.with(|c| c.set(false));
    ALLOCS.load(Ordering::Relaxed) - before
}

/// A libfec codeword (`(i·a + b) mod 2^m` data + libfec's check symbols; see
/// `fec/rs_tests.rs`).
fn codeword(k: usize, (a, b): (usize, usize), m: usize, parity: &str) -> Vec<u16> {
    let mut c: Vec<u16> = (0..k).map(|i| ((i * a + b) % (1 << m)) as u16).collect();
    c.extend(
        (0..parity.len())
            .step_by(2)
            .map(|i| u16::from_str_radix(&parity[i..i + 2], 16).unwrap()),
    );
    c
}

fn bits(syms: &[u16], m: usize) -> Vec<u8> {
    syms.iter()
        .flat_map(|&s| (0..m).rev().map(move |b| (s >> b) as u8 & 1))
        .collect()
}

/// `depth` copies of `cw` interleaved, with `errors[c]` symbol errors in copy `c`.
fn block(cw: &[u16], depth: usize, errors: &[usize], m: usize) -> Vec<u8> {
    let n = cw.len();
    let mut syms = vec![0u16; depth * n];
    for c in 0..depth {
        for j in 0..n {
            let e = if j < errors[c] {
                1 + (j as u16 * 7) % ((1 << m) - 1)
            } else {
                0
            };
            syms[j * depth + c] = cw[j] ^ e;
        }
    }
    bits(&syms, m)
}

struct Case {
    params: Value,
    cw: Vec<u16>,
    m: usize,
    depth: usize,
}

#[test]
fn reed_solomon_process_allocates_nothing_in_steady_state() {
    let ccsds = codeword(
        223,
        (7, 3),
        8,
        "0c29d565dd7fb9654ba23207f8d9962e04699784e7e28d63c017bad54b582bc4",
    );
    let ccsds_params = |depth: usize| {
        json!({"n": 255, "k": 223, "poly": "0x187", "fcr": 112, "prim": 11,
               "dual_basis": true, "depth": depth})
    };
    let cases = [
        Case {
            params: ccsds_params(1),
            cw: ccsds.clone(),
            m: 8,
            depth: 1,
        },
        Case {
            params: ccsds_params(4),
            cw: ccsds,
            m: 8,
            depth: 4,
        },
        Case {
            params: json!({"n": 24, "k": 12, "symbol_bits": 6, "poly": "0x43", "fcr": 1,
                           "strip": false}),
            cw: codeword(12, (5, 1), 6, "38190b1735190d2b3b330331"),
            m: 6,
            depth: 1,
        },
    ];
    for case in &cases {
        let t = (case.cw.len() - if case.m == 8 { 223 } else { 12 }) / 2;
        let d = case.depth;
        let frames: Vec<Vec<u8>> = vec![
            block(&case.cw, d, &vec![0; d], case.m),
            block(&case.cw, d, &vec![t; d], case.m),
            block(&case.cw, d, &vec![t + 1; d], case.m),
            block(&case.cw, d, &vec![0; d], case.m)[..20].to_vec(),
        ];
        let maps = BTreeMap::new();
        let ctx = BuildCtx {
            field_maps: &maps,
            input_types: &[PortType::Frames],
        };
        let mut b = Registry::builtin()
            .build("reed_solomon", case.params.as_object().unwrap(), &ctx)
            .unwrap_or_else(|e| panic!("{e}"));
        let info = PortInfo {
            ty: PortType::Frames,
            rate_hz: 10.0,
            max_items: frames.len(),
            hold_items: 0,
        };
        let outs = b.init(&[info]).unwrap();
        let mut outputs: Vec<Output> = outs.iter().map(Output::for_port).collect();
        let bytes: usize = frames.iter().map(|f| f.len().div_ceil(8)).sum();
        let mut input = FrameBuf::with_capacity(frames.len(), bytes);
        for (i, f) in frames.iter().enumerate() {
            input.push_bits(f, FrameInfo::new(i as u64, i as u64 * 100, 0));
        }
        let mut chunk = |k: usize, outputs: &mut Vec<Output>| {
            let meta = ChunkMeta {
                index: (k * frames.len()) as u64,
                source_index: (k * frames.len() * 100) as f64,
                source_per_item: 100.0,
                flags: if k == 0 {
                    ChunkFlags::DISCONTINUITY
                } else {
                    ChunkFlags::NONE
                },
                ..ChunkMeta::start(10.0)
            };
            let inputs = [Input {
                meta,
                data: PortSlice::Frames(&input),
            }];
            for o in outputs.iter_mut() {
                o.begin_chunk();
            }
            b.process(&mut Io::new(&inputs, outputs)).expect("process");
        };
        chunk(0, &mut outputs);
        let n = allocations(|| {
            for k in 1..12 {
                chunk(k, &mut outputs);
            }
        });
        assert_eq!(n, 0, "{}: {n} allocations", case.params);
        let PortVec::Frames(out) = &outputs[0].data else {
            panic!("frames")
        };
        let checks: Vec<CrcStatus> = out.iter().map(|f| f.info.check).collect();
        assert_eq!(
            checks,
            [
                CrcStatus::Valid,
                CrcStatus::Valid,
                CrcStatus::Invalid,
                CrcStatus::Invalid
            ],
            "{}",
            case.params
        );
    }
}
