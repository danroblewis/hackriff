//! The PFB and DDC hot paths (T-008) allocate nothing in steady state. Own test binary: it
//! installs a counting global allocator.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use hk_core::Discontinuity;
use hk_dsp::synth::{self, Rng};
use hk_dsp::{Ddc, DdcSpec, InputInfo, Pfb, PfbConfig, ResampleKind};

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
fn pfb_and_ddc_do_not_allocate_per_block() {
    let fs = 2e6;
    let prov = provenance(433.92e6, fs);
    let block = 8192;
    let mut rng = Rng::new(5);
    let f32_block = synth::complex_noise(&mut rng, block, 1e-2);
    let (i8_block, _) = synth::quantize_ci8(&f32_block);

    let mut pfb = Pfb::new(PfbConfig::new(64)).unwrap();
    let mut pfb_subset = Pfb::new(PfbConfig {
        active: Some(vec![1, 5, 40]),
        ..PfbConfig::new(256)
    })
    .unwrap();
    let mut ddcs: Vec<Ddc> = [
        DdcSpec::new(200e3, 100e3).with_output_rate(1e6),
        DdcSpec::new(312_345.6, 40e3).with_output_rate(50e3),
        DdcSpec::new(-412_345.6, 30e3).with_output_rate(48e3),
        DdcSpec::new(555_555.5, 30e3).with_output_rate(47_123.4),
    ]
    .into_iter()
    .map(|s| Ddc::new(s, fs).unwrap())
    .collect();
    let kinds: Vec<_> = ddcs
        .iter()
        .map(|d| d.plan().resample.as_ref().map(|r| r.kind))
        .collect();
    assert!(kinds.iter().any(|k| k.is_none()));
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Some(ResampleKind::Integer { .. })))
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Some(ResampleKind::Rational { .. })))
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, Some(ResampleKind::Fractional { .. })))
    );

    let mut next = 0u64;
    let mut outputs = 0usize;
    let mut step = |i: usize, counting: bool, outputs: &mut usize| {
        let flags = if next == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        let h = header(next, &prov, flags);
        let info = InputInfo::from(&h);
        COUNTING.with(|c| c.set(counting));
        if i % 2 == 0 {
            *outputs += pfb.process(info, &f32_block).frames;
            *outputs += pfb_subset.process(info, &f32_block).frames;
            for d in &mut ddcs {
                *outputs += d.process(info, &f32_block).unwrap().samples.len();
            }
        } else {
            *outputs += pfb.process(info, &i8_block).frames;
            *outputs += pfb_subset.process(info, &i8_block).frames;
            for d in &mut ddcs {
                *outputs += d.process(info, &i8_block).unwrap().samples.len();
            }
        }
        COUNTING.with(|c| c.set(false));
        next += block as u64;
    };
    for i in 0..4 {
        step(i, false, &mut outputs);
    }
    ALLOCATIONS.store(0, Ordering::Relaxed);
    outputs = 0;
    for i in 0..60 {
        step(i, true, &mut outputs);
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    assert!(outputs > 10_000, "{outputs}");
    assert_eq!(allocations, 0, "hot path allocated {allocations} times");
}
