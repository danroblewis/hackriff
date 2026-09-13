//! Folding frames allocates nothing in steady state (T-017 build step 5). Runs in its own test
//! binary because it installs a counting global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use hk_model::{PowerUnit, Timestamp};
use hk_store::{FrameInput, Pyramid, PyramidConfig};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator and only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn ingest_is_allocation_free_in_steady_state() {
    const S: i64 = 1_000_000_000;
    const FPS: i64 = 32; // exact 31.25 ms frames: every 1-s column gets the same count
    let t0 = 1_789_300_800 * S;
    let dir = std::env::temp_dir().join(format!("hk-store-noalloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = PyramidConfig {
        checkpoint_interval: None,
        ..PyramidConfig::default()
    };
    let mut p = Pyramid::open(&dir, cfg).unwrap();
    let bins = 4096;
    let psd: Vec<f32> = (0..bins)
        .map(|i| 1e-14 * (1.0 + (i % 17) as f32 * 0.1))
        .collect();
    let peak: Vec<f32> = psd.iter().map(|v| v * 2.0).collect();
    let fs = 20e6;
    let frame = |k: i64| {
        let mut f = FrameInput::new(
            Timestamp::from_unix_nanos(t0 + k * S / FPS),
            S / FPS,
            433.92e6 - fs / 2.0,
            fs / bins as f64,
            PowerUnit::Dbfs,
            &psd,
        );
        f.peak = Some(&peak);
        f
    };
    // Warm-up: tiles opened, regrid plan built, column buffers sized (several columns closed).
    for k in 0..5 * FPS {
        p.ingest(&frame(k)).unwrap();
    }
    let before = ALLOCS.load(Ordering::Relaxed);
    // 50 s more inside the same 1-minute level-0 tiles: 1600 frames, 50 column closes.
    for k in 5 * FPS..55 * FPS {
        p.ingest(&frame(k)).unwrap();
    }
    let allocations = ALLOCS.load(Ordering::Relaxed) - before;
    assert_eq!(p.stats().tiles_written, 0, "no seal inside the window");
    assert_eq!(
        allocations, 0,
        "steady-state ingest allocated {allocations} times"
    );
    drop(p);
    let _ = std::fs::remove_dir_all(&dir);
}
