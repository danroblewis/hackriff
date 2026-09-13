//! The STFT hot path (`StftProcessor::push`, including frame emission, SK, holds and
//! persistence) allocates nothing in steady state. Own test binary: it installs a counting global
//! allocator.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use hk_core::Discontinuity;
use hk_dsp::synth::{self, Rng};
use hk_dsp::{InputInfo, PersistenceConfig, StftConfig, StftProcessor, WelchConfig};

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

#[test]
fn stft_push_does_not_allocate_in_steady_state() {
    let fs = 2e6;
    let prov = provenance(144e6, fs);
    let mut config = StftConfig::new(WelchConfig::new(1024), 8);
    config.persistence = Some(PersistenceConfig::default());
    let mut stft = StftProcessor::new(config).unwrap();

    let mut rng = Rng::new(1);
    let f32_block = synth::complex_noise(&mut rng, 3000, 1e-2);
    let (i8_block, _) = synth::quantize_ci8(&f32_block);

    // Warm up: first input creates the frame.
    let h = header(0, &prov, Discontinuity::STREAM_START);
    stft.push(InputInfo::from(&h), &f32_block, |_| {});

    let mut frames = 0usize;
    let mut next = 3000u64;
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    for i in 0..200 {
        let h = header(next, &prov, Discontinuity::NONE);
        if i % 2 == 0 {
            stft.push(InputInfo::from(&h), &f32_block, |_| frames += 1);
        } else {
            stft.push(InputInfo::from(&h), &i8_block, |_| frames += 1);
        }
        next += 3000;
    }
    COUNTING.with(|c| c.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    assert!(frames > 50);
    assert_eq!(allocations, 0, "push allocated {allocations} times");
}
