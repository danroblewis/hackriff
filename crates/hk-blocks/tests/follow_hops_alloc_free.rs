//! T-093 (ADR-0011 §1.4 rule 1): the `follow_hops` merge allocates nothing per chunk in steady
//! state — reordering, deduplication and release reuse the buffers sized at `init`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use hk_blocks::BuildCtx;
use hk_blocks::{
    ChunkFlags, ChunkMeta, FrameBuf, FrameInfo, Input, Io, Output, PortInfo, PortSlice, Registry,
};
use hk_recipe::PortType;
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

#[test]
fn the_merge_allocates_nothing_per_chunk() {
    let ctx = BuildCtx {
        field_maps: &Default::default(),
        input_types: &[PortType::Frames],
    };
    let mut block = Registry::builtin()
        .build(
            "follow_hops",
            json!({"dedupe_s": 0.05, "order_window_s": 0.02})
                .as_object()
                .unwrap(),
            &ctx,
        )
        .unwrap();
    let port = PortInfo {
        ty: PortType::Frames,
        rate_hz: 100.0,
        max_items: 16,
        hold_items: 0,
    };
    let out_port = block.init(&[port]).unwrap();
    let mut outputs = [Output::for_port(&out_port[0])];
    // Source rate 10 kHz: window 200 samples, dedupe 500. Three channels, a duplicate of every
    // third message on the neighbouring channel, a 40-byte frame each.
    let mut input = FrameBuf::with_capacity(16, 16 * 64);
    let mut run_chunk = |k: u64, input: &mut FrameBuf, outputs: &mut [Output; 1]| {
        input.clear();
        let t = k * 100;
        for ch in 0..3u16 {
            let mut bytes = [0u8; 40];
            bytes[0] = (k % 251) as u8;
            bytes[1] = ch as u8;
            let mut info = FrameInfo::new(0, t + u64::from(ch) * 7, ch);
            info.bit_len = 320;
            input.push(&bytes, info);
        }
        if k % 3 == 0 {
            let mut bytes = [0u8; 40];
            bytes[0] = (k % 251) as u8;
            let mut info = FrameInfo::new(0, t + 3, 1);
            info.bit_len = 320;
            input.push(&bytes, info);
        }
        let meta = ChunkMeta {
            source_index: (t + 100) as f64,
            source_per_item: 100.0,
            flags: ChunkFlags::NONE,
            ..ChunkMeta::start(100.0)
        };
        outputs[0].begin_chunk();
        let inputs = [Input {
            meta,
            data: PortSlice::Frames(input),
        }];
        block.process(&mut Io::new(&inputs, outputs)).unwrap();
        outputs[0].data.len()
    };
    let mut warm_out = 0;
    for k in 0..400 {
        warm_out += run_chunk(k, &mut input, &mut outputs);
    }
    assert!(warm_out > 1000, "frames flow: {warm_out}");
    ALLOCS.store(0, Ordering::Relaxed);
    COUNT_HERE.with(|c| c.set(true));
    let mut out = 0;
    for k in 400..2400 {
        out += run_chunk(k, &mut input, &mut outputs);
    }
    COUNT_HERE.with(|c| c.set(false));
    assert!(out > 5000, "frames flow: {out}");
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "allocations in steady state"
    );
    let dups = block
        .status()
        .extra
        .iter()
        .find(|(k, _)| *k == "dups")
        .unwrap()
        .1;
    assert!(dups > 600.0, "duplicates removed: {dups}");
}
