//! The steady-state sample path allocates nothing: source `read_block`, ring `push` and reader
//! `read` run under a counting global allocator. (Its own test binary, so the allocator does not
//! affect other tests.)

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use hk_core::{
    Pacing, ReadOutcome, ReplayOptions, RingConfig, SigmfReplaySource, Source, ring_buffer,
};
use hk_model::sigmf::Datatype;
use num_complex::{Complex, Complex32};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn note() {
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards every call to the system allocator unchanged; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn counted<R>(f: impl FnOnce() -> R) -> (R, usize) {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    (r, ALLOCATIONS.load(Ordering::Relaxed))
}

#[test]
fn replay_push_and_read_do_not_allocate_per_block() {
    const BLOCK: usize = 4096;
    const BLOCKS: usize = 2000;
    let mut meta = meta(Datatype::Ci8, 2e6);
    meta.global.provenance = Some(provenance("synthetic:no-alloc", 1e8, 2e6));
    let data = ramp_ci8(BLOCK * BLOCKS);
    let options = ReplayOptions {
        block_len: BLOCK,
        pacing: Pacing::Unpaced,
    };

    // Complex32 path: source -> ring -> reader. The ring (2^16 samples) wraps ~125 times.
    let mut source =
        SigmfReplaySource::from_reader(meta.clone(), Cursor::new(data.clone()), options).unwrap();
    let (mut writer, ring) = ring_buffer::<Complex32>(RingConfig {
        sample_capacity: 1 << 16,
        block_capacity: 64,
    });
    let mut reader = ring.reader();
    let mut block = Vec::with_capacity(BLOCK);
    let mut out = vec![Complex32::default(); BLOCK];
    // Warm-up: first block registers the provenance with the ring and the reader cache.
    let header = source.read_block(&mut block).unwrap().unwrap();
    writer.push(&header, &block).unwrap();
    assert!(matches!(reader.read(&mut out), ReadOutcome::Data(_)));

    let (blocks, allocations) = counted(|| {
        let mut n = 1;
        while let Some(header) = source.read_block(&mut block).unwrap() {
            writer.push(&header, &block).unwrap();
            match reader.read(&mut out) {
                ReadOutcome::Data(chunk) => assert_eq!(chunk.len, BLOCK),
                other => panic!("{other:?}"),
            }
            n += 1;
        }
        n
    });
    assert_eq!(blocks, BLOCKS);
    assert_eq!(allocations, 0, "Complex32 hot path allocated");

    // Native ci8 path into a Complex<i8> ring.
    let mut source = SigmfReplaySource::from_reader(meta, Cursor::new(data), options).unwrap();
    let (mut writer, ring) = ring_buffer::<Complex<i8>>(RingConfig {
        sample_capacity: 1 << 16,
        block_capacity: 64,
    });
    let mut reader = ring.reader();
    let mut block = Vec::with_capacity(BLOCK);
    let mut out = vec![Complex::<i8>::default(); BLOCK];
    let header = source.read_block_ci8(&mut block).unwrap().unwrap();
    writer.push(&header, &block).unwrap();
    assert!(matches!(reader.read(&mut out), ReadOutcome::Data(_)));
    let (_, allocations) = counted(|| {
        while let Some(header) = source.read_block_ci8(&mut block).unwrap() {
            writer.push(&header, &block).unwrap();
            assert!(matches!(reader.read(&mut out), ReadOutcome::Data(_)));
        }
    });
    assert_eq!(allocations, 0, "ci8 hot path allocated");
}
