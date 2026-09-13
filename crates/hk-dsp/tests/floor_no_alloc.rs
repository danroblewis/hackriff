//! The noise-floor hot path allocates nothing in steady state: `NoiseFloorTracker::update` through
//! warm-up, impulsive frames, gate releases, rise/end/fall events, episode ends on reset and
//! segment resets, plus per-channel floors, threshold levels, minimum statistics and block
//! percentiles. Own test binary: it installs a counting global allocator.

mod common;
mod floor_common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use floor_common::*;
use hk_core::Discontinuity;
use hk_dsp::floor::{
    BlockPercentile, ChannelFloor, FloorChangeConfig, FloorConfig, FloorEventKind, FloorKind,
    FloorThreshold, ImpulsiveGateConfig, MinStatConfig, MinStatistics, NoiseFloorTracker,
    PercentileConfig,
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
    let step = vec![1.4f32; bins];

    // Frame period 5.12 ms; short durations so one 160-frame cycle exercises every path:
    // rise (20) → end (40..) → impulsive (70) → sub-threshold step with gate release (80..95)
    // → rise (100) closed by a gain reset (120) → fall after a hot warm-up → reset (140) → gap (150).
    let defaults = FloorConfig::default();
    let cfg = FloorConfig {
        change: FloorChangeConfig {
            confirm_s: 0.05,
            end_s: 0.05,
            holdoff_s: 0.05,
            ..defaults.change
        },
        impulsive: ImpulsiveGateConfig {
            max_duration_s: 0.02,
            ..defaults.impulsive
        },
        ..defaults
    };
    let mut frames = Vec::new();
    for k in 0..160 {
        src.provenance = if (120..140).contains(&k) {
            high.clone()
        } else {
            low.clone()
        };
        let profile = match k {
            20..40 | 70 | 100..130 => &ten,
            80..95 => &step,
            _ => &one,
        };
        let flags = if k == 150 {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        let mut f = src.empty_frame();
        src.fill(&mut f, profile, flags);
        frames.push(f);
    }

    let mut tracker = NoiseFloorTracker::new(cfg).unwrap();
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
    let threshold = FloorThreshold::s4(10.0);
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
    let mut kinds = [0usize; 3];

    let mut cycle = |kinds: &mut [usize; 3]| {
        for f in &frames {
            let ff = tracker.update(f, |e| {
                kinds[match e.kind {
                    FloorEventKind::Rise => 0,
                    FloorEventKind::End => 1,
                    FloorEventKind::Fall => 2,
                }] += 1;
            });
            ff.channel_floors(&channels, FloorKind::Wide, &mut channel_out);
            threshold.write_on_levels(&ff.wide_floor, &mut levels);
            threshold.write_guard_levels(&ff.wide_floor, &mut levels);
            minstat.update(&f.spectrum.psd);
            percentile.estimate_block(&f.spectrum.psd[..256]);
        }
    };
    cycle(&mut kinds); // warm-up: the first frame sizes every buffer

    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    for _ in 0..3 {
        cycle(&mut kinds);
    }
    COUNTING.with(|c| c.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let stats = tracker.stats();
    eprintln!("events (rise, end, fall) {kinds:?}; {stats:?}");
    assert_eq!(
        allocations, 0,
        "floor hot path allocated {allocations} times"
    );
    assert!(kinds[0] >= 6 && kinds[1] >= 6, "rise/end events exercised");
    assert!(stats.impulsive_frames >= 3, "impulsive frames exercised");
    assert!(stats.gate_releases >= 6, "gate releases exercised");
    assert!(
        stats.resets >= 9,
        "segment resets exercised ({})",
        stats.resets
    );
}
