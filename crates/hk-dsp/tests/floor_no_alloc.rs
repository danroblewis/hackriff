//! The noise-floor hot path allocates nothing in steady state: `NoiseFloorTracker::update` through
//! warm-up, shape learning, impulsive frames, gate releases, every event kind (rise, extend,
//! merge, update, end, fall, unknown), comparable and incomparable resets, plus per-channel
//! floors, threshold levels, minimum statistics and block percentiles. Own test binary: it
//! installs a counting global allocator.

mod common;
mod floor_common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use floor_common::*;
use hk_core::Discontinuity;
use hk_dsp::floor::{
    BlockPercentile, ChannelFloor, EndReason, FloorChangeConfig, FloorConfig, FloorEventKind,
    FloorKind, FloorThreshold, ImpulsiveGateConfig, MinStatConfig, MinStatistics,
    NoiseFloorTracker, PercentileConfig,
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

const KINDS: [FloorEventKind; 6] = [
    FloorEventKind::Rise,
    FloorEventKind::Extend,
    FloorEventKind::Update,
    FloorEventKind::End,
    FloorEventKind::Fall,
    FloorEventKind::Unknown,
];

#[test]
fn floor_hot_path_does_not_allocate_in_steady_state() {
    let bins = 1024;
    let low = provenance_with(1575.42e6, 2e6, 16.0);
    let high = provenance_with(1575.42e6, 2e6, 24.0);
    let mut src = GammaFrames::new(bins, 10, low.clone(), 11);
    let ten = 10.0f32;

    // Frame period 5.12 ms; confirm/end/hold-off 10 frames, so one 220-frame cycle exercises:
    // A (0..400) at 20..70 and B (620..1024) at 26..80 → rise, rise; a bridge (380..640) at
    // 40..70 → merge (End Merged + Extend); A and the bridge off at 70 → Update; B off at 80 →
    // End; an impulsive frame (100); a sub-threshold step with gate release (110..125); a band
    // rise (130..) closed by a gain change (150, Unknown); a fall after the hot warm-up (160..);
    // back to low gain (175, Unknown-free reset); a rise (185..) carried across a gap (200) and
    // ended in the next cycle.
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
    let mut profile = vec![1.0f32; bins];
    for k in 0..220 {
        src.provenance = if (150..175).contains(&k) {
            high.clone()
        } else {
            low.clone()
        };
        profile.fill(1.0);
        if (20..70).contains(&k) {
            profile[..400].fill(ten);
        }
        if (26..80).contains(&k) {
            profile[620..].fill(ten);
        }
        if (40..70).contains(&k) {
            profile[380..640].fill(ten);
        }
        if k == 100 || (130..165).contains(&k) || k >= 185 {
            profile.fill(ten);
        }
        if (110..125).contains(&k) {
            profile.fill(1.4);
        }
        let flags = if k == 200 {
            Discontinuity::GAP
        } else {
            Discontinuity::NONE
        };
        let mut f = src.empty_frame();
        src.fill(&mut f, &profile, flags);
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
    let mut kinds = [0usize; 6];
    let mut merges = 0usize;

    let mut cycle = |kinds: &mut [usize; 6], merges: &mut usize| {
        for f in &frames {
            let ff = tracker.update(f, |e| {
                kinds[KINDS.iter().position(|&k| k == e.kind).unwrap()] += 1;
                *merges += usize::from(e.end_reason == Some(EndReason::Merged));
            });
            ff.channel_floors(&channels, FloorKind::Wide, &mut channel_out);
            threshold.write_on_levels(&ff.wide_floor, &mut levels);
            threshold.write_guard_levels(&ff.wide_floor, &mut levels);
            minstat.update(&f.spectrum.psd);
            percentile.estimate_block(&f.spectrum.psd[..256]);
        }
    };
    cycle(&mut kinds, &mut merges); // warm-up: the first frame sizes every buffer

    ALLOCATIONS.store(0, Ordering::Relaxed);
    kinds = [0; 6];
    merges = 0;
    COUNTING.with(|c| c.set(true));
    for _ in 0..3 {
        cycle(&mut kinds, &mut merges);
    }
    COUNTING.with(|c| c.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let stats = tracker.stats();
    eprintln!(
        "events over 3 cycles (rise, extend, update, end, fall, unknown) {kinds:?}, merges {merges}; {stats:?}"
    );
    assert_eq!(
        allocations, 0,
        "floor hot path allocated {allocations} times"
    );
    assert!(kinds.iter().all(|&n| n >= 3), "every event kind exercised");
    assert!(merges >= 3, "merges exercised");
    assert!(stats.impulsive_frames >= 3, "impulsive frames exercised");
    assert!(stats.gate_releases >= 3, "gate releases exercised");
    assert!(
        stats.resets >= 12,
        "segment resets exercised ({})",
        stats.resets
    );
}
