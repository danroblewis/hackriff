//! T-127: T-120 review follow-ups on the bandit scheduler, one test each: gap-free pass coverage
//! under floor deferrals, the UCB NaN guard, released leases and intent leaving the floor window,
//! interactive steps exempt from floor violations, dropped verification candidates counted, and
//! dwell counters rolled back on a cut.

mod sched_common;

use std::sync::Arc;

use hk_core::ScheduleStep;
use hk_core::scheduler::{Clock, Lease, Purpose, Scheduler, SyntheticClock, UserIntent};
use hk_model::ScanPolicy;
use hk_model::attention::observation::LeaseKind;
use hk_model::attention::schedule::BanditConfig;
use hk_model::attention::score::SharedInterestingness;
use sched_common::*;
use serde_json::json;

const S: i64 = 1_000_000_000;

fn sched(cfg: BanditConfig) -> (Scheduler<SyntheticClock>, Arc<SharedInterestingness>) {
    let p = plan(
        "bandit-followups",
        1,
        vec![
            region(420.0, 450.0, 1.0, None),
            region(902.0, 928.0, 1.0, None),
        ],
        ScanPolicy::SweepThenDwell,
        vec![],
        json!({}),
    );
    let mut s = hackrf(&p);
    let provider = Arc::new(SharedInterestingness::default());
    s.enable_bandit(cfg, Arc::clone(&provider) as _).unwrap();
    let now = s.clock().now();
    publish_candidates(
        &provider,
        now,
        vec![
            candidate(1, 433.92, 50.0, 0.9, false),
            candidate(2, 915.2, 50.0, 0.7, false),
        ],
    );
    (s, provider)
}

/// Next step after re-packing, clock advanced by its duration.
fn step(s: &mut Scheduler<SyntheticClock>) -> ScheduleStep {
    s.refresh_bandit();
    let st = s.next_step();
    s.clock().advance_ns(st.duration_ns);
    st
}

fn dwell_counts(s: &Scheduler<SyntheticClock>) -> u64 {
    let c = s.attention_status().bandit.unwrap().counters;
    c.exploit_dwells + c.explore_dwells + c.beacon_dwells
}

#[test]
fn passes_stay_gap_free_with_the_bandit_on_and_floor_deferrals() {
    let (mut s, _p) = sched(BanditConfig {
        sweep_floor: 0.6,
        ..BanditConfig::default()
    });
    let n = s.plan().hops.len() as u32;
    assert!(n >= 2);
    let mut hops = Vec::new();
    for _ in 0..6_000 {
        if let Purpose::Sweep { hop } = step(&mut s).purpose {
            hops.push(hop);
        }
    }
    let st = s.attention_status();
    let b = st.bandit.unwrap();
    assert!(b.counters.floor_deferrals > 0, "{b:?}");
    assert!(s.stats().bandit_steps > 0);
    assert_eq!(hops[0], 0);
    for w in hops.windows(2) {
        assert_eq!(w[1], (w[0] + 1) % n, "a pass skipped or repeated a hop");
    }
    assert!(s.passes_completed() >= 3);
    let total = st.discovery_s + st.exploit_s + st.explore_s + st.other_s;
    assert!(st.discovery_s >= (0.6 - 0.05) * total, "{st:?}");
}

#[test]
fn a_fully_suspect_arm_without_pseudo_dwell_has_index_zero_not_nan() {
    let (mut s, provider) = sched(BanditConfig {
        prior_pseudo_dwell_s: 0.0,
        ..BanditConfig::default()
    });
    let mut suspect = candidate(3, 440.0, 50.0, 0.8, false);
    suspect.suspect_fraction = 1.0;
    publish_candidates(&provider, s.clock().now(), vec![suspect]);
    s.refresh_bandit();
    let arms = s.arm_table();
    assert!(arms.iter().all(|a| !a.ucb.is_nan()), "{arms:?}");
    let a = arms
        .iter()
        .find(|a| a.active && a.suspect_fraction == 1.0)
        .expect("the suspect window is packed");
    assert_eq!(a.ucb, 0.0);
    for _ in 0..500 {
        step(&mut s);
    }
    assert!(s.arm_table().iter().all(|a| !a.ucb.is_nan()));
}

/// T-127 review: updating a lease trims its running step when the lease changes; renewing it
/// unchanged leaves the running step (and its accounting) alone.
#[test]
fn a_changed_lease_update_trims_its_running_step_and_a_renewal_does_not() {
    let (mut s, _p) = sched(BanditConfig::default());
    for _ in 0..20 {
        step(&mut s);
    }
    let lease = Lease {
        id: 1,
        kind: LeaseKind::UserPin,
        center_hz: 915e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: None,
    };
    s.add_lease(lease).unwrap();
    s.refresh_bandit();
    let st = s.next_step();
    assert!(matches!(st.purpose, Purpose::Lease { .. }));
    s.clock().advance_ns(st.duration_ns / 4);
    let before = s.attention_status().other_s;
    s.add_lease(lease).unwrap();
    assert_eq!(
        s.running_end(),
        st.t_end(),
        "a renewal keeps the running step"
    );
    assert_eq!(s.attention_status().other_s, before);

    s.add_lease(Lease {
        center_hz: 916e6,
        ..lease
    })
    .unwrap();
    assert_eq!(s.running_end(), s.clock().now(), "a changed lease trims it");
    let after = s.attention_status().other_s;
    let unrun = (st.duration_ns - st.duration_ns / 4) as f64 / 1e9;
    assert!(
        (before - after - unrun).abs() < 1e-6,
        "{before} {after} {unrun}"
    );
}

#[test]
fn released_leases_and_intent_take_their_unrun_time_out_of_the_floor_window() {
    let (mut s, _p) = sched(BanditConfig::default());
    for _ in 0..20 {
        step(&mut s);
    }
    let lease = Lease {
        id: 1,
        kind: LeaseKind::UserPin,
        center_hz: 915e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: None,
    };
    s.add_lease(lease).unwrap();
    s.refresh_bandit();
    let st = s.next_step();
    assert!(matches!(st.purpose, Purpose::Lease { .. }));
    let before = s.attention_status().other_s;
    s.clock().advance_ns(st.duration_ns / 4);
    assert!(s.release_lease(1));
    let after = s.attention_status().other_s;
    let unrun = (st.duration_ns - st.duration_ns / 4) as f64 / 1e9;
    assert!(
        (before - after - unrun).abs() < 1e-6,
        "{before} {after} {unrun}"
    );

    s.preempt(UserIntent {
        id: 2,
        center_hz: 433e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: Some(60 * S),
    })
    .unwrap();
    let st = s.next_step();
    assert!(matches!(st.purpose, Purpose::UserIntent { .. }));
    let before = s.attention_status().other_s;
    s.clock().advance_ns(st.duration_ns / 4);
    assert!(s.release_intent());
    let after = s.attention_status().other_s;
    let unrun = (st.duration_ns - st.duration_ns / 4) as f64 / 1e9;
    assert!(
        (before - after - unrun).abs() < 1e-6,
        "{before} {after} {unrun}"
    );
}

#[test]
fn interactive_steps_never_count_as_floor_violations_but_leases_do() {
    let (mut s, _p) = sched(BanditConfig::default());
    s.preempt(UserIntent {
        id: 1,
        center_hz: 433e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: None,
    })
    .unwrap();
    let mut t = 0;
    while t < 900 * S {
        let st = step(&mut s);
        assert!(matches!(st.purpose, Purpose::UserIntent { .. }));
        t += st.duration_ns;
    }
    assert!(!s.attention_status().sweep_floor_met);
    assert_eq!(s.stats().floor_violations, 0);
    s.release_intent();
    s.add_lease(Lease {
        id: 3,
        kind: LeaseKind::Decoder,
        center_hz: 915e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: None,
    })
    .unwrap();
    for _ in 0..5 {
        step(&mut s);
    }
    assert!(s.stats().floor_violations > 0);
}

#[test]
fn suspects_beyond_the_verification_slots_are_counted_as_dropped() {
    let (mut s, provider) = sched(BanditConfig::default());
    let max = hk_core::scheduler::bandit::MAX_VERIFICATIONS;
    let flagged: Vec<_> = (0..max as i64 + 6)
        .map(|i| candidate(100 + i, 420.5 + 0.2 * i as f64, 25.0, 0.5, true))
        .collect();
    publish_candidates(&provider, s.clock().now(), flagged);
    assert!(s.refresh_bandit());
    let b = s.attention_status().bandit.unwrap();
    assert_eq!(b.counters.verifications_dropped, 6, "{b:?}");
    assert_eq!(b.pending_verifications, max);
}

#[test]
fn a_cut_bandit_dwell_rolls_back_its_counters() {
    let (mut s, _p) = sched(BanditConfig::default());
    let st = loop {
        s.refresh_bandit();
        let st = s.next_step();
        if matches!(st.purpose, Purpose::Bandit { .. }) {
            break st;
        }
        s.clock().advance_ns(st.duration_ns);
    };
    let during = dwell_counts(&s);
    let visits = s.arm_table().iter().map(|a| a.visits).sum::<u64>();
    s.clock().advance_ns(st.duration_ns / 2);
    s.preempt(UserIntent {
        id: 9,
        center_hz: 433e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: Some(S),
    })
    .unwrap();
    assert_eq!(dwell_counts(&s), during - 1);
    assert_eq!(
        s.arm_table().iter().map(|a| a.visits).sum::<u64>(),
        visits - 1
    );
}
