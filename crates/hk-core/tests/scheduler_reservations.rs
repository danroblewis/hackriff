//! T-276 (C04 ← C29): reserved windows — a predicted satellite pass or launch window booked
//! ahead of time. The scheduler keeps the window free by clipping lower tiers at its start, begins
//! it as a pinned lease exactly there, ends it at its predicted end, and states every way it can
//! lose window time (interactive intent, a full lease table) rather than hiding it. All time is
//! the synthetic (device) clock.

mod sched_common;

use hk_core::ScheduleStep;
use hk_core::scheduler::{
    Clock, Lease, MAX_LEASES, Purpose, Reservation, Scheduler, SchedulerError, SyntheticClock,
    UserIntent,
};
use hk_model::attention::observation::{LeaseKind, Tier};
use hk_model::{ScanPolicy, Timestamp};
use sched_common::*;
use serde_json::Value;

fn sweep_sched() -> Scheduler<SyntheticClock> {
    let p = plan(
        "reserve",
        1,
        vec![region(88.0, 148.0, 1.0, None)],
        ScanPolicy::SweepThenDwell,
        vec![],
        Value::Null,
    );
    hackrf(&p)
}

fn pass(id: u64, start_ns: i64, duration_ns: i64) -> Reservation {
    Reservation {
        lease: Lease {
            id,
            kind: LeaseKind::Pass,
            center_hz: 137.5e6,
            rate_hz: 2.4e6,
            gains: None,
            duration_ns: Some(duration_ns),
        },
        start: Timestamp::from_unix_nanos(start_ns),
    }
}

fn step(s: &mut Scheduler<SyntheticClock>) -> ScheduleStep {
    let st = s.next_step();
    s.clock().advance_ns(st.duration_ns);
    st
}

fn is_pass(st: &ScheduleStep, id: u64) -> bool {
    st.purpose
        == Purpose::Lease {
            kind: LeaseKind::Pass,
            lease: id,
        }
}

#[test]
fn a_reserved_pass_is_kept_free_begun_at_its_start_and_ended_at_its_end() {
    let mut s = sweep_sched();
    let start = T0_NS + 10 * S + 312_345_679;
    let end = start + 5 * S + 250_000_000;
    s.reserve(pass(7, start, end - start)).unwrap();
    assert_eq!(s.reservations().count(), 1);
    assert_eq!(s.attention_status().reservations, 1);

    // Before the window: ordinary discovery, and the step that would cross it ends exactly there.
    let mut before = Vec::new();
    loop {
        let st = step(&mut s);
        assert!(st.purpose.is_discovery(), "{st:?}");
        assert!(
            st.t_end().as_unix_nanos() <= start,
            "{st:?} runs into the window"
        );
        let done = st.t_end().as_unix_nanos() == start;
        before.push(st);
        if done {
            break;
        }
    }
    let clipped = *before.last().unwrap();
    let Purpose::Sweep { hop: clipped_hop } = clipped.purpose else {
        panic!("{clipped:?}")
    };
    assert!(clipped.duration_ns < before[0].duration_ns);
    assert_eq!(s.stats().reservation_clips, 1);

    // The window: pinned-lease steps at the reserved tuning, contiguous from start to end.
    let mut t = start;
    while t < end {
        let st = step(&mut s);
        assert!(is_pass(&st, 7), "{st:?}");
        assert_eq!(st.purpose.tier(), Tier::PinnedLease);
        assert_eq!((st.center_hz, st.rate_hz), (137.5e6, 2.4e6));
        assert_eq!(st.t_start.as_unix_nanos(), t);
        t = st.t_end().as_unix_nanos();
    }
    assert_eq!(t, end, "the last pass step ends exactly at the window end");
    assert_eq!(s.reservations().count(), 0);

    // After: discovery resumes with the clipped hop, in full (rolled back, not left short).
    let resumed = step(&mut s);
    assert_eq!(resumed.purpose, Purpose::Sweep { hop: clipped_hop });
    assert_eq!(resumed.duration_ns, before[0].duration_ns);
    assert_eq!(s.leases().count(), 0, "the pass lease expired at its end");
    let stats = s.stats();
    assert_eq!(
        (
            stats.reservations_started,
            stats.reservations_late,
            stats.reservations_missed
        ),
        (1, 0, 0)
    );
    assert_eq!(stats.truncated_slots, 1);
}

#[test]
fn interactive_intent_outranks_a_pass_and_the_lost_time_is_disclosed() {
    let mut s = sweep_sched();
    let clk = s.clock().clone();
    let start = T0_NS + 3 * S + S / 4;
    let end = start + 10 * S;
    s.reserve(pass(8, start, end - start)).unwrap();
    s.preempt(UserIntent {
        id: 1,
        center_hz: 101.1e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: Some(5 * S + S / 2),
    })
    .unwrap();
    // Intent is never clipped for a reservation: it holds its 1 s slices straight through start.
    let mut held = Vec::new();
    while clk.now().as_unix_nanos() < T0_NS + 5 * S + S / 2 {
        held.push(step(&mut s));
    }
    assert!(
        held.iter()
            .all(|st| st.purpose == Purpose::UserIntent { intent: 1 })
    );
    assert!(
        held.iter()
            .any(|st| st.t_start.as_unix_nanos() < start && st.t_end().as_unix_nanos() > start)
    );
    // The pass begins late, at the first boundary after intent, and still ends at its own end.
    let first = step(&mut s);
    assert!(is_pass(&first, 8), "{first:?}");
    assert_eq!(first.t_start.as_unix_nanos(), T0_NS + 5 * S + S / 2);
    let mut last = first;
    while s.leases().count() > 0 && last.t_end().as_unix_nanos() < end {
        last = step(&mut s);
        assert!(is_pass(&last, 8), "{last:?}");
    }
    assert_eq!(last.t_end().as_unix_nanos(), end);
    let stats = s.stats();
    assert_eq!(
        (stats.reservations_started, stats.reservations_late),
        (1, 1)
    );
    assert!(step(&mut s).purpose.is_discovery());
}

#[test]
fn a_pass_shares_the_pinned_tier_round_robin_with_other_leases() {
    let mut s = sweep_sched();
    s.add_lease(Lease {
        id: 3,
        kind: LeaseKind::UserPin,
        center_hz: 433.92e6,
        rate_hz: 2e6,
        gains: None,
        duration_ns: None,
    })
    .unwrap();
    let start = T0_NS + 2 * S + S / 3;
    s.reserve(pass(9, start, 6 * S)).unwrap();
    let pre: Vec<_> = (0..3).map(|_| step(&mut s)).collect();
    assert!(pre.iter().all(|st| st.purpose.tier() == Tier::PinnedLease));
    // The user pin's slice is clipped at the pass start too, then the two alternate.
    assert_eq!(pre.last().unwrap().t_end().as_unix_nanos(), start);
    let window: Vec<_> = (0..6).map(|_| step(&mut s)).collect();
    let ids: Vec<u64> = window
        .iter()
        .map(|st| match st.purpose {
            Purpose::Lease { lease, .. } => lease,
            p => panic!("{p:?}"),
        })
        .collect();
    assert!(ids.windows(2).all(|w| w[0] != w[1]), "round robin: {ids:?}");
    assert!(ids.contains(&9) && ids.contains(&3));
}

#[test]
fn reserving_inside_the_running_step_trims_it_and_cancelling_restores_the_slot() {
    let mut s = sweep_sched();
    let clk = s.clock().clone();
    let first = s.next_step();
    let Purpose::Sweep { hop } = first.purpose else {
        panic!("{first:?}")
    };
    let start = first.t_start.as_unix_nanos() + first.duration_ns / 2;
    s.reserve(pass(11, start, 4 * S)).unwrap();
    assert_eq!(
        s.running_end().as_unix_nanos(),
        start,
        "the running step now ends at the window start"
    );
    // Cancelled before it begins: the trimmed hop is revisited in full.
    assert!(s.cancel_reservation(11));
    assert!(!s.cancel_reservation(11));
    clk.set(Timestamp::from_unix_nanos(start));
    let again = s.next_step();
    assert_eq!(
        (again.purpose, again.duration_ns),
        (Purpose::Sweep { hop }, first.duration_ns)
    );
    assert_eq!(s.stats().reservations_started, 0);
}

#[test]
fn a_window_that_cannot_begin_is_counted_missed_never_silently_dropped() {
    let mut s = sweep_sched();
    for id in 0..MAX_LEASES as u64 {
        s.add_lease(Lease {
            id: 100 + id,
            kind: LeaseKind::Decoder,
            center_hz: 100e6 + id as f64 * 1e6,
            rate_hz: 2e6,
            gains: None,
            duration_ns: None,
        })
        .unwrap();
    }
    let start = T0_NS + S;
    s.reserve(pass(12, start, 3 * S)).unwrap();
    while s.clock().now().as_unix_nanos() < start + 4 * S {
        let st = step(&mut s);
        assert!(!is_pass(&st, 12));
    }
    let stats = s.stats();
    assert_eq!(
        (stats.reservations_started, stats.reservations_missed),
        (0, 1)
    );
    assert_eq!(s.reservations().count(), 0);
}

#[test]
fn reservations_are_validated() {
    let mut s = sweep_sched();
    let ok = pass(1, T0_NS + S, S);
    let no_duration = Reservation {
        lease: Lease {
            duration_ns: None,
            ..ok.lease
        },
        ..ok
    };
    let out_of_range = Reservation {
        lease: Lease {
            center_hz: 10e9,
            ..ok.lease
        },
        ..ok
    };
    let past = pass(1, T0_NS - 5 * S, S);
    for bad in [no_duration, out_of_range, past] {
        assert!(
            matches!(s.reserve(bad), Err(SchedulerError::OutOfCapability { .. })),
            "{bad:?}"
        );
    }
    s.reserve(ok).unwrap();
    // The same id replaces, never duplicates.
    s.reserve(pass(1, T0_NS + 2 * S, S)).unwrap();
    assert_eq!(s.reservations().count(), 1);
    assert_eq!(
        s.reservations().next().unwrap().start.as_unix_nanos(),
        T0_NS + 2 * S
    );
}
