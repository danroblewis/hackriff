//! T-115 observation recorder over real scheduler steps: sweep records per pass with the hop
//! order of the schedule, one geometry, dwell records per step, the 60 s cap, and no allocation
//! per hop or dwell in the steady state. (Its own test binary: the counting allocator.)

mod sched_common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use hk_core::scheduler::observe::{ObservationRecorder, StepObservation, WindowRule};
use hk_core::scheduler::{Purpose, ScheduleStep};
use hk_model::attention::observation::{ObservationRecord, Reason, Tier};
use hk_model::{ScanPolicy, Timestamp};
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

const RULE: WindowRule = WindowRule {
    fft_bins: 4096,
    dc_half_hz: 15e3,
    enbw_bins: 1.5,
};

fn applied(step: &ScheduleStep) -> StepObservation {
    StepObservation {
        step: *step,
        center_hz: step.center_hz,
        rate_hz: step.rate_hz,
        settled: Some(step.t_start),
        end: step.t_end(),
        dropped_samples: 0,
        overload: false,
    }
}

fn sweep_plan() -> hk_model::ScanPlan {
    plan(
        "observe",
        1,
        vec![region(400.0, 460.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        json!({}),
    )
}

#[test]
fn scheduler_observe_records_one_sweep_record_per_pass_in_schedule_order() {
    let p = sweep_plan();
    let mut s = hackrf(&p);
    let n = s.plan().hops.len();
    assert!(n >= 3, "plan has {n} hops");
    let steps = run(&mut s, 3 * n);
    let mut rec = ObservationRecorder::new(s.plan(), RULE, None, None);
    let mut out = Vec::new();
    for st in &steps {
        rec.observe(&applied(st), &mut |r| out.push(r));
    }
    rec.flush(&mut |r| out.push(r));
    for r in &out {
        r.validate().unwrap();
    }
    let ObservationRecord::Geometry(g) = &out[0] else {
        panic!("geometry first: {:?}", out[0]);
    };
    // T-173: odd passes tune the DC-dithered centres, recorded against their own geometry. Each
    // geometry is written once, before the first record that uses it.
    let geometries: Vec<_> = out
        .iter()
        .filter_map(|r| match r {
            ObservationRecord::Geometry(g) => Some(g),
            _ => None,
        })
        .collect();
    assert_eq!(
        geometries.len(),
        2,
        "each pass parity's geometry written once"
    );
    for (pass, geometry) in geometries.iter().enumerate() {
        assert_eq!(geometry.hops.len(), n);
        for (hop, w) in s.plan().hops.iter().zip(&geometry.hops) {
            assert_eq!(
                *w,
                RULE.window(hop.center_on_pass(pass as u64), hop.rate_hz)
            );
        }
    }
    assert_ne!(geometries[0].id, geometries[1].id);
    let sweeps: Vec<_> = out
        .iter()
        .filter_map(|r| match r {
            ObservationRecord::Sweep(x) => Some(x),
            _ => None,
        })
        .collect();
    assert_eq!(sweeps.len(), 3, "one record per pass");
    let mut i = 0;
    for (pass, sw) in sweeps.iter().enumerate() {
        assert_eq!(sw.geometry, geometries[pass % 2].id);
        assert_eq!(g.id, geometries[0].id);
        assert_eq!(sw.visits.len(), n);
        assert_eq!(sw.span.start, steps[i].t_start);
        for (k, v) in sw.visits.iter().enumerate() {
            let st = &steps[i];
            let Purpose::Sweep { hop } = st.purpose else {
                panic!("sweep-only plan");
            };
            assert_eq!(v.hop, hop);
            assert_eq!(v.hop as usize, k);
            assert_eq!(
                i64::from(v.start_ms) * 1_000_000,
                st.t_start.as_unix_nanos() - sw.span.start.as_unix_nanos()
            );
            assert_eq!(i64::from(v.observed_ms) * 1_000_000, st.duration_ns);
            i += 1;
        }
        assert_eq!(sw.span.end, steps[i - 1].t_end());
    }
}

#[test]
fn scheduler_observe_dwells_are_one_record_each_with_reason_tier_and_preemption() {
    let p = sweep_plan();
    let mut s = hackrf(&p);
    let base = run(&mut s, 1)[0];
    let mut step = base;
    step.purpose = Purpose::UserIntent { intent: 9 };
    step.center_hz = 433.92e6;
    let mut o = applied(&step);
    o.settled = Some(step.t_start.saturating_add_nanos(5_000_000));
    o.end = step.t_start.saturating_add_nanos(step.duration_ns / 2);
    let mut rec = ObservationRecorder::new(s.plan(), RULE, None, None);
    let mut out = Vec::new();
    rec.observe(&o, &mut |r| out.push(r));
    let [ObservationRecord::Dwell(d)] = out.as_slice() else {
        panic!("one dwell record: {out:?}");
    };
    d.validate().unwrap();
    assert_eq!(d.reason, Reason::Interactive { intent: 9 });
    assert_eq!(d.tier, Tier::Interactive);
    assert!(d.preempted);
    assert_eq!(d.observed.start, o.settled.unwrap());
    assert_eq!(d.observed.end, o.end);
    assert_eq!(d.window, RULE.window(433.92e6, step.rate_hz));

    // Never settled: an empty observed interval at the end.
    let mut never = applied(&step);
    never.settled = None;
    out.clear();
    rec.observe(&never, &mut |r| out.push(r));
    let ObservationRecord::Dwell(d) = &out[0] else {
        panic!()
    };
    assert_eq!(d.observed.duration_ns(), 0);
    assert!(!d.preempted);
}

#[test]
fn scheduler_observe_sweep_records_never_span_more_than_60_s() {
    let p = sweep_plan();
    let mut s = hackrf(&p);
    let n = s.plan().hops.len();
    let steps = run(&mut s, n);
    let mut rec = ObservationRecorder::new(s.plan(), RULE, None, None);
    let mut out = Vec::new();
    // Stretch every hop to 25 s: a record closes before a visit would pass 60 s.
    for (k, st) in steps.iter().enumerate() {
        let mut st = *st;
        st.t_start = Timestamp::from_unix_nanos(T0_NS + k as i64 * 25 * S);
        st.duration_ns = 25 * S;
        rec.observe(&applied(&st), &mut |r| out.push(r));
    }
    rec.flush(&mut |r| out.push(r));
    let sweeps: Vec<_> = out
        .iter()
        .filter_map(|r| match r {
            ObservationRecord::Sweep(x) => Some(x),
            _ => None,
        })
        .collect();
    assert!(sweeps.iter().all(|s| s.validate().is_ok()));
    assert!(sweeps.iter().all(|s| s.visits.len() <= 2));
    assert_eq!(sweeps.iter().map(|s| s.visits.len()).sum::<usize>(), n);
}

#[test]
fn scheduler_observe_a_long_intent_closes_the_open_sweep_record_when_it_begins() {
    let p = sweep_plan();
    let mut s = hackrf(&p);
    let n = s.plan().hops.len();
    let steps = run(&mut s, n);
    let half = (n / 2).max(1);
    let last = steps[half - 1];
    let mut intent = last;
    intent.seq = last.seq + 1;
    intent.t_start = last.t_end();
    intent.duration_ns = 2 * 3600 * S;
    intent.purpose = Purpose::UserIntent { intent: 7 };
    let sweeps = |out: &[ObservationRecord]| {
        out.iter()
            .filter(|r| matches!(r, ObservationRecord::Sweep(_)))
            .count()
    };

    // The pipeline calls `begin` as the intent starts: the half pass is emitted then, not 2 h
    // later with the next hop.
    let mut rec = ObservationRecorder::new(s.plan(), RULE, None, None);
    let mut out = Vec::new();
    for st in &steps[..half] {
        rec.observe(&applied(st), &mut |r| out.push(r));
    }
    assert_eq!(sweeps(&out), 0);
    rec.begin(&intent, &mut |r| out.push(r));
    let Some(ObservationRecord::Sweep(sw)) = out.last() else {
        panic!("the open sweep closed when the intent began: {out:?}");
    };
    sw.validate().unwrap();
    assert_eq!(sw.visits.len(), half);
    assert_eq!(sw.span.end, last.t_end());
    rec.observe(&applied(&intent), &mut |r| out.push(r));
    assert!(matches!(out.last(), Some(ObservationRecord::Dwell(d)) if d.tier == Tier::Interactive));
    assert_eq!(sweeps(&out), 1);

    // A short dwell leaves the pass open; a caller that never calls `begin` still gets the sweep
    // before the long step's record.
    let mut rec = ObservationRecorder::new(s.plan(), RULE, None, None);
    let mut out = Vec::new();
    for st in &steps[..half] {
        rec.observe(&applied(st), &mut |r| out.push(r));
    }
    let mut short = intent;
    short.duration_ns = S;
    rec.begin(&short, &mut |r| out.push(r));
    rec.observe(&applied(&short), &mut |r| out.push(r));
    assert_eq!(sweeps(&out), 0);
    let mut long = intent;
    long.seq = short.seq + 1;
    long.t_start = short.t_end();
    rec.observe(&applied(&long), &mut |r| out.push(r));
    let k = out.len();
    assert!(matches!(out[k - 2], ObservationRecord::Sweep(_)));
    assert!(matches!(out[k - 1], ObservationRecord::Dwell(_)));
}

#[test]
fn scheduler_observe_steady_state_hops_and_dwells_do_not_allocate() {
    let p = sweep_plan();
    let mut s = hackrf(&p);
    let n = s.plan().hops.len();
    let steps = run(&mut s, 2 * n + 1);
    let mut rec = ObservationRecorder::new(s.plan(), RULE, None, None);
    let mut emitted = 0usize;
    // Warm up: the geometry and the first pass; hop 0 of pass 2 closes pass 1.
    for st in &steps[..=n] {
        rec.observe(&applied(st), &mut |_| emitted += 1);
    }
    let obs: Vec<StepObservation> = steps[n + 1..2 * n].iter().map(applied).collect();
    let mut dwell = obs[0];
    dwell.step.purpose = Purpose::UserIntent { intent: 1 };
    let (_, allocs) = counted(|| {
        for o in &obs {
            rec.observe(o, &mut |_| {});
            rec.observe(&dwell, &mut |_| {});
        }
    });
    assert!(emitted >= 2);
    assert_eq!(allocs, 0, "observe allocated in the steady state");
}
