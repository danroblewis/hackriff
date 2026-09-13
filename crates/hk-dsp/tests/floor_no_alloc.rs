//! The noise-floor hot path (`NoiseFloorTracker::update` with segment resets, impulsive frames
//! and floor-rise events, per-channel floors, threshold levels, minimum statistics and block
//! percentiles) allocates nothing in steady state. Own test binary: it installs a counting
//! global allocator.

mod common;
mod floor_common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use floor_common::*;
use hk_core::Discontinuity;
use hk_dsp::floor::{
    BlockPercentile, ChannelFloor, FloorConfig, FloorKind, FloorThreshold, MinStatConfig,
    MinStatistics, NoiseFloorTracker, PercentileConfig,
};

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
fn floor_hot_path_does_not_allocate_in_steady_state() {
    let bins = 1024;
    let low = provenance_with(1575.42e6, 2e6, 16.0);
    let high = provenance_with(1575.42e6, 2e6, 24.0);
    let mut src = GammaFrames::new(bins, 10, low.clone(), 11);
    let one = vec![1.0f32; bins];
    let ten = vec![10.0f32; bins];

    // A cycle with a floor rise (then a fall), an impulsive frame, a gain change and a gap.
    let mut frames = Vec::new();
    for k in 0..80 {
        src.provenance = if (60..70).contains(&k) {
            high.clone()
        } else {
            low.clone()
        };
        let profile = if (20..35).contains(&k) || k == 50 {
            &ten
        } else {
            &one
        };
        let flags = if k == 75 {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        let mut f = src.empty_frame();
        src.fill(&mut f, profile, flags);
        frames.push(f);
    }

    let mut tracker = NoiseFloorTracker::new(FloorConfig::default()).unwrap();
    let mut minstat = MinStatistics::new(
        MinStatConfig {
            window_frames: 32,
            ..MinStatConfig::default()
        },
        bins,
        10.0,
    )
    .unwrap();
    let mut percentile = BlockPercentile::new(PercentileConfig::default(), 10.0).unwrap();
    let threshold = FloorThreshold::new(10.0, 1e-6);
    let channels = [0..100, 300..700, 900..1024];
    let mut channel_out = [ChannelFloor {
        start_bin: 0,
        end_bin: 0,
        floor: 0.0,
        dbfs_per_hz: 0.0,
        dbfs: 0.0,
        uncertainty_db: 0.0,
        quantisation_limited: false,
    }; 3];
    let mut levels = vec![0.0f32; bins];
    let mut events = 0usize;

    let mut cycle = |events: &mut usize| {
        for f in &frames {
            let ff = tracker.update(f, |_| *events += 1);
            ff.channel_floors(&channels, FloorKind::Frame, &mut channel_out);
            threshold.write_levels(&ff.floor, &mut levels);
            minstat.update(&f.spectrum.psd);
            percentile.estimate_block(&f.spectrum.psd[..256]);
        }
    };
    cycle(&mut events); // warm-up: the first frame sizes every buffer

    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    for _ in 0..3 {
        cycle(&mut events);
    }
    COUNTING.with(|c| c.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(
        allocations, 0,
        "floor hot path allocated {allocations} times"
    );
    let stats = tracker.stats();
    assert!(events >= 4, "rise events exercised ({events})");
    assert!(stats.impulsive_frames >= 4, "impulsive frames exercised");
    assert!(
        stats.resets >= 4 * 3,
        "segment resets exercised ({})",
        stats.resets
    );
}
