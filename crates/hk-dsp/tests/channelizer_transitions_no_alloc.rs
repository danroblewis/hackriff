//! The PFB and DDC (T-008) allocate nothing across stream transitions: gaps, retunes, gain
//! changes and zero-length blocks. A sample-rate change is the one documented exception: the
//! DDC re-plans its filters and a raster-offset PFB rebuilds its shifted taps; the plain PFB
//! does not allocate, and all return to zero allocations on the next block. Own test binary:
//! it installs a counting global allocator.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use hk_core::{BlockHeader, Discontinuity};
use hk_dsp::synth::{self, Rng};
use hk_dsp::{Ddc, DdcSpec, InputInfo, Pfb, PfbConfig};
use num_complex::Complex32;

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

fn counted(f: impl FnOnce()) -> usize {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    ALLOCATIONS.load(Ordering::Relaxed)
}

struct Rig {
    pfb: Pfb,
    pfb_raster: Pfb,
    ddcs: Vec<Ddc>,
}

impl Rig {
    /// Allocation counts for (plain PFB, raster PFB, DDCs) on one block.
    fn step(&mut self, h: &BlockHeader, x: &[Complex32]) -> [usize; 3] {
        let info = InputInfo::from(h);
        let a = counted(|| {
            let _ = self.pfb.process(info, x).frames;
        });
        let b = counted(|| {
            let _ = self.pfb_raster.process(info, x).frames;
        });
        let c = counted(|| {
            for d in &mut self.ddcs {
                let _ = d.process(info, x).unwrap().samples.len();
            }
        });
        [a, b, c]
    }
}

#[test]
fn transitions_do_not_allocate_except_rate_change_replan() {
    let p2 = provenance(433.92e6, 2e6);
    let retuned = provenance(434.1e6, 2e6);
    let louder = provenance_with(433.92e6, 2e6, 32.0);
    let p4 = provenance(433.92e6, 4e6);
    let block = 8192;
    let mut rng = Rng::new(77);
    let x = synth::complex_noise(&mut rng, block, 1e-2);
    let mut rig = Rig {
        pfb: Pfb::new(PfbConfig::new(64)).unwrap(),
        pfb_raster: Pfb::new(PfbConfig {
            raster_offset_hz: 7_000.0,
            ..PfbConfig::new(100)
        })
        .unwrap(),
        ddcs: [
            DdcSpec::new(200e3, 100e3).with_output_rate(1e6),
            DdcSpec::new(312_345.6, 40e3).with_output_rate(50e3),
            DdcSpec::new(-412_345.6, 30e3).with_output_rate(48e3),
            DdcSpec::new(555_555.5, 30e3).with_output_rate(47_123.4),
        ]
        .into_iter()
        .map(|s| Ddc::new(s, 2e6).unwrap())
        .collect(),
    };

    let mut idx = 0u64;
    for i in 0..4 {
        let flags = if i == 0 {
            Discontinuity::STREAM_START
        } else {
            Discontinuity::NONE
        };
        rig.step(&header(idx, &p2, flags), &x);
        idx += block as u64;
    }

    let check = |label: &str, counts: [usize; 3], want_zero: [bool; 3]| {
        eprintln!("{label:<34} allocations (pfb, raster pfb, ddcs) = {counts:?}");
        for (k, (&n, &zero)) in counts.iter().zip(&want_zero).enumerate() {
            if zero {
                assert_eq!(n, 0, "{label}: processor group {k} allocated {n} times");
            }
        }
    };
    let all = [true; 3];

    check(
        "steady",
        rig.step(&header(idx, &p2, Discontinuity::NONE), &x),
        all,
    );
    idx += block as u64;

    idx += 5_000; // gap
    check(
        "gap",
        rig.step(&header(idx, &p2, Discontinuity::NONE), &x),
        all,
    );
    idx += block as u64;
    check(
        "after gap",
        rig.step(&header(idx, &p2, Discontinuity::NONE), &x),
        all,
    );
    idx += block as u64;

    check(
        "retune",
        rig.step(&header(idx, &retuned, Discontinuity::NONE), &x),
        all,
    );
    idx += block as u64;
    check(
        "gain change",
        rig.step(&header(idx, &louder, Discontinuity::GAIN_CHANGE), &x),
        all,
    );
    idx += block as u64;

    check(
        "zero-length block",
        rig.step(&header(idx, &louder, Discontinuity::NONE), &[]),
        all,
    );
    check(
        "after zero-length",
        rig.step(&header(idx, &louder, Discontinuity::NONE), &x),
        all,
    );
    idx += block as u64;

    // Rate change: the plain PFB still allocates nothing; the raster PFB rebuilds its taps and
    // the DDCs re-plan (documented allocations on a discontinuity, not steady state).
    let counts = rig.step(&header(idx, &p4, Discontinuity::NONE), &x);
    check("rate change 2 -> 4 Msps", counts, [true, false, false]);
    assert!(
        counts[1] > 0 && counts[2] > 0,
        "expected re-plan allocations: {counts:?}"
    );
    idx += block as u64;
    for i in 0..3 {
        check(
            &format!("steady at 4 Msps #{i}"),
            rig.step(&header(idx, &p4, Discontinuity::NONE), &x),
            all,
        );
        idx += block as u64;
    }
}
