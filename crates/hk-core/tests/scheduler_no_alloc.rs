//! T-009: the scheduler's steady state allocates nothing: `next_step`, `preempt`,
//! `release_intent`, POI updates and `StepApplier::apply`, including verification groups and
//! cut slots, run under a counting global allocator. (Own test binary.)

mod sched_common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hk_core::scheduler::{Poi, StepApplier, UserIntent};
use hk_core::{Gains, SourceCapabilities, SourceControl, SourceError};
use hk_model::ScanPolicy;
use sched_common::*;
use serde_json::json;

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

/// Accepts every control.
struct NullControl(SourceCapabilities);

impl SourceControl for NullControl {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.0
    }
    fn tune(&self, _: f64) -> Result<(), SourceError> {
        Ok(())
    }
    fn set_sample_rate(&self, _: f64) -> Result<(), SourceError> {
        Ok(())
    }
    fn set_gains(&self, _: &Gains) -> Result<(), SourceError> {
        Ok(())
    }
    fn set_baseband_filter(&self, _: f64) -> Result<(), SourceError> {
        Ok(())
    }
    fn set_bias_tee(&self, _: bool) -> Result<(), SourceError> {
        Ok(())
    }
    fn start(&self) -> Result<(), SourceError> {
        Ok(())
    }
    fn stop(&self) -> Result<(), SourceError> {
        Ok(())
    }
}

fn poi(key: u64, interestingness: f64, verify: bool) -> Poi {
    Poi {
        key,
        center_hz: 902e6 + 3e6 * key as f64,
        bandwidth_hz: 200e3,
        interestingness,
        burst_interval_ns: Some(S),
        verify,
    }
}

#[test]
fn scheduling_and_applying_steps_do_not_allocate() {
    let p = plan(
        "no-alloc",
        1,
        vec![
            region(902.0, 928.0, 2.0, Some(60.0)),
            region(2400.0, 2500.0, 1.0, None),
        ],
        ScanPolicy::SweepThenDwell,
        vec![],
        json!({ "scheduler": { "sweeps_per_cycle": 2, "rate_change": true } }),
    );
    let mut s = hackrf(&p);
    for key in 0..8 {
        s.offer_poi(poi(key, 1.0 + key as f64, key % 2 == 0))
            .unwrap();
    }
    let clk = s.clock().clone();
    let mut applier = StepApplier::new(Arc::new(NullControl(SourceCapabilities::hackrf_one())));
    for _ in 0..100 {
        let st = s.next_step();
        clk.advance_ns(st.duration_ns);
        applier.apply(&st).unwrap();
    }
    let intent = UserIntent {
        id: 1,
        center_hz: 915e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: Some(3 * S),
    };

    let ((), allocations) = counted(|| {
        for i in 0..20_000u64 {
            let st = s.next_step();
            applier.apply(&st).unwrap();
            let half = st.duration_ns / 2;
            clk.advance_ns(half);
            if i % 997 == 0 {
                s.preempt(intent).unwrap();
            } else if i % 1499 == 0 {
                s.preempt(UserIntent {
                    duration_ns: None,
                    ..intent
                })
                .unwrap();
                s.release_intent();
            } else {
                clk.advance_ns(st.duration_ns - half);
            }
            if i % 1500 == 0 {
                s.offer_poi(poi(3, 0.5 + (i % 7) as f64, false)).unwrap();
                s.remove_poi(100);
            }
        }
    });
    assert_eq!(allocations, 0, "allocations in the steady state");
    let stats = s.stats();
    assert!(stats.verifications_completed > 0, "{stats:?}");
    assert!(stats.intent_steps > 0 && stats.truncated_slots > 0 && stats.trust_steps > 0);
}
