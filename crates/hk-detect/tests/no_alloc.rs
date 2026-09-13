//! `Detector::process` allocates nothing in steady state except the emitted detections (one
//! `detector_version` string each), through gain changes, retunes, gaps, impulsive runs, clipped
//! frames, max-duration splits, gap merges and integrated evaluations with a comb. Own test
//! binary: it installs a counting global allocator.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_detect::{ClipCount, Detector, DetectorConfig, DetectorEvent};
use hk_model::SurveyId;

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

struct Handles {
    a: ProvenanceHandle,
    b: ProvenanceHandle,
    c: ProvenanceHandle,
}

/// One pass of the scenario; returns (allocations inside `process`, detections emitted).
/// `(frame, allocations, detections)` for frames that allocated more than they emitted.
type Offenders = Vec<(u64, usize, usize)>;

fn pass(det: &mut Detector, h: &Handles, bins: usize, count: bool) -> (usize, usize, Offenders) {
    let mut offenders = Offenders::with_capacity(64);
    let mut src = GammaFrames::new(bins, N_AVG, h.a.clone(), 99);
    let mut frame = src.empty_frame();
    let mut floor = floor_frame(&frame, &flat(bins), 0);
    // Profiles, built before counting.
    let mut base = flat(bins);
    for k in 0..8 {
        add_fractional_line(&mut base, 300.0 + 42.9 * k as f64, 12.0); // comb
    }
    add_line(&mut base, 1300, 3, 20.0); // continuous carrier (splits at max duration)
    let mut burst = base.clone();
    add_line(&mut burst, 1600, 8, 15.0);
    let mut imp = base.clone();
    for k in 0..12 {
        add_line(&mut imp, 1700 + 20 * k, 3, 15.0);
    }
    let samples = u64::from(N_AVG) * bins as u64;
    let mut allocs = 0usize;
    let mut detections = 0usize;
    let mut segment = 0u64;
    for i in 0..700u64 {
        let mut flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        match i {
            250 => {
                src.provenance = h.b.clone();
                segment += 1;
                flags = Discontinuity::GAIN_CHANGE;
            }
            420 => {
                src.provenance = h.c.clone();
                segment += 1;
                flags = Discontinuity::RETUNE;
            }
            560 => {
                src.sample_index += 777;
                segment += 1;
                flags = Discontinuity::GAP;
            }
            _ => {}
        }
        let impulsive = (100..106).contains(&i) || (480..485).contains(&i);
        let profile = if impulsive {
            &imp
        } else if i % 40 < 6 {
            &burst
        } else {
            &base
        };
        let clip = if (50..60).contains(&i) {
            ClipCount::new(900, samples)
        } else {
            ClipCount::new(1, samples)
        };
        src.fill(&mut frame, profile, flags);
        refresh_floor(&mut floor, &frame, segment, impulsive);
        let emitted_before = detections;
        let mut sink = |e: DetectorEvent<'_>| {
            let was = COUNTING.with(|c| c.replace(false));
            if let DetectorEvent::Detection(d) = e {
                detections += 1;
                std::hint::black_box(&d);
                drop(d);
            }
            COUNTING.with(|c| c.set(was));
        };
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        COUNTING.with(|c| c.set(count));
        det.process(&frame, &floor, clip, &mut sink);
        COUNTING.with(|c| c.set(false));
        let a = ALLOCATIONS.load(Ordering::Relaxed) - before;
        allocs += a;
        let emitted = detections - emitted_before;
        if count && a > emitted && offenders.len() < offenders.capacity() {
            offenders.push((i, a, emitted));
        }
    }
    (allocs, detections, offenders)
}

#[test]
fn detector_hot_path_allocates_only_emitted_detections() {
    let bins = 2048;
    let fs = 10e6;
    let h = Handles {
        a: provenance(915e6, fs, 24.0),
        b: provenance(915e6, fs, 32.0),
        c: provenance(916e6, fs, 32.0),
    };
    let mut config = DetectorConfig::new(SurveyId::new());
    config.max_duration_s = 0.1;
    config.integration.block_s = 0.05;
    config.rules.comb.trials = 50;
    let mut det = Detector::new(config).unwrap();
    // Warm-up passes reach every high-water mark (pools, extents, link lists, comb chance cache).
    // Slot capacities only grow (below the 1024-bin shrink threshold), but which slot a component
    // lands in depends on the free list, so they converge over a few passes (the third here).
    let mut warm = 0;
    for k in 0..4 {
        warm = pass(&mut det, &h, bins, false).1;
        eprintln!(
            "warm-up pass {k}: labeller memory {} bytes",
            det.labeler_memory_bytes()
        );
    }
    let (allocs, detections, offenders) = pass(&mut det, &h, bins, true);
    eprintln!(
        "frames allocating beyond their detections (frame, allocations, detections): {offenders:?}"
    );
    let s = det.stats();
    eprintln!(
        "second pass: {allocs} allocations for {detections} detections (warm-up {warm}); segments {}, impulsive events {}, confirmations {}, evaluations {}",
        s.segments, s.impulsive_events, s.confirmations, s.evaluations
    );
    assert!(detections > 20 && s.impulsive_events >= 2 && s.evaluations > 10);
    assert!(
        allocs <= detections,
        "{allocs} allocations for {detections} emitted detections"
    );
}
