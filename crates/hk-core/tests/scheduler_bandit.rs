//! T-120 (ADR-0012 §5): the bandit revisit policy through the scheduler — preemption order,
//! one-shot suspect verification, UCB exploitation inside the exploration and sweep floors,
//! determinism, the low-power profile and exact POI accounting from the scheduler's windows.
//! All time is the synthetic (device) clock.

mod sched_common;

use std::sync::Arc;

use hk_core::ScheduleStep;
use hk_core::scheduler::{
    BanditKind, Clock, Lease, Purpose, ScheduledDwell, Scheduler, SyntheticClock, UserIntent,
};
use hk_model::attention::observation::{LeaseKind, Tier};
use hk_model::attention::schedule::{BanditConfig, DwellOutcome, nominal_poi};
use hk_model::attention::score::SharedInterestingness;
use hk_model::{FreqRange, ScanPolicy, TimeRange, Timestamp};
use sched_common::*;
use serde_json::json;

fn bandit_sched() -> (Scheduler<SyntheticClock>, Arc<SharedInterestingness>) {
    let p = plan(
        "bandit",
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
    s.enable_bandit(BanditConfig::default(), Arc::clone(&provider) as _)
        .unwrap();
    (s, provider)
}

fn step(s: &mut Scheduler<SyntheticClock>) -> ScheduleStep {
    s.refresh_bandit();
    let st = s.next_step();
    s.clock().advance_ns(st.duration_ns);
    st
}

#[test]
fn preemption_order_is_interactive_then_leases_then_scheduled_then_bandit_then_sweep() {
    let (mut s, provider) = bandit_sched();
    let clk = s.clock().clone();
    publish_candidates(
        &provider,
        clk.now(),
        vec![candidate(1, 433.92, 50.0, 0.9, false)],
    );
    let hops = s.plan().hops.len() as u32;

    // Bandit > sweep: a bandit dwell interrupts the pass, which then resumes where it was.
    let (mut last_hop, mut saw_bandit) = (None, false);
    for _ in 0..5_000 {
        let st = step(&mut s);
        match st.purpose {
            Purpose::Sweep { hop } if saw_bandit => {
                assert_eq!(Some(hop), last_hop.map(|h| (h + 1) % hops));
                break;
            }
            Purpose::Sweep { hop } => last_hop = Some(hop),
            Purpose::Bandit { .. } => {
                assert_eq!(st.purpose.tier(), Tier::Bandit);
                saw_bandit = true;
            }
            _ => {}
        }
    }
    assert!(saw_bandit && last_hop.is_some());

    // Scheduled plan > bandit and sweep.
    let now = clk.now();
    s.schedule_dwell(ScheduledDwell {
        id: 5,
        center_hz: 440e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: 2 * S,
        due: now,
        every_ns: None,
    })
    .unwrap();
    s.refresh_bandit();
    let st = s.next_step();
    assert_eq!(
        (st.purpose, st.purpose.tier()),
        (Purpose::Scheduled { target: 5 }, Tier::ScheduledPlan)
    );
    clk.advance_ns(S);
    // Pinned lease > scheduled plan: the running scheduled dwell is cut.
    let lease = Lease {
        id: 9,
        kind: LeaseKind::UserPin,
        center_hz: 915e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: None,
    };
    s.add_lease(lease).unwrap();
    s.refresh_bandit();
    let st = s.next_step();
    assert_eq!(st.purpose.tier(), Tier::PinnedLease);
    assert_eq!(
        st.purpose,
        Purpose::Lease {
            kind: LeaseKind::UserPin,
            lease: 9
        }
    );
    clk.advance_ns(S / 2);
    // Interactive > pinned lease.
    s.preempt(UserIntent {
        id: 3,
        center_hz: 100e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: None,
    })
    .unwrap();
    let st = step(&mut s);
    assert_eq!(
        (st.purpose, st.purpose.tier()),
        (Purpose::UserIntent { intent: 3 }, Tier::Interactive)
    );
    assert!(s.release_intent());
    assert_eq!(step(&mut s).purpose.tier(), Tier::PinnedLease);
    assert!(s.release_lease(9));
    // The cut scheduled dwell was rolled back: it runs again, in full.
    let st = step(&mut s);
    assert_eq!(
        (st.purpose, st.duration_ns),
        (Purpose::Scheduled { target: 5 }, 2 * S)
    );
    let st = step(&mut s);
    assert!(
        matches!(st.purpose.tier(), Tier::Bandit | Tier::BackgroundSweep),
        "{st:?}"
    );
    let stats = s.stats();
    assert!(stats.lease_steps >= 2 && stats.scheduled_steps == 2 && stats.truncated_slots >= 1);
    assert!(
        s.request_tx_slot(&hk_core::scheduler::TxSlotRequest {
            center_hz: 915e6,
            duration_ns: S
        })
        .is_err()
    );
}

#[test]
fn a_suspect_gets_one_verification_group_then_is_banned_and_scheduled_dwells_wait_for_it() {
    let (mut s, provider) = bandit_sched();
    let clk = s.clock().clone();
    let set = || {
        vec![
            candidate(1, 433.92, 50.0, 0.6, false),
            candidate(2, 915.2, 50.0, 0.9, true),
        ]
    };
    publish_candidates(&provider, clk.now(), set());
    let ghost = candidate_key(2);
    let first = (0..10_000)
        .map(|_| step(&mut s))
        .find(|st| st.verification_group.is_some())
        .expect("a verification group");
    assert_eq!(first.purpose.poi(), Some(ghost));
    assert_eq!(first.purpose.tier(), Tier::Bandit);
    // A scheduled dwell due mid-group waits for the group (atomic within its tier).
    s.schedule_dwell(ScheduledDwell {
        id: 1,
        center_hz: 440e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: S,
        due: clk.now(),
        every_ns: None,
    })
    .unwrap();
    let mut group = vec![first];
    loop {
        let st = step(&mut s);
        if st.verification_group == first.verification_group {
            group.push(st);
        } else {
            assert_eq!(st.purpose, Purpose::Scheduled { target: 1 });
            break;
        }
    }
    assert!(
        group.len() >= 3 && group.iter().all(|g| g.purpose.poi() == Some(ghost)),
        "{group:?}"
    );
    // Re-scored after the group and still flagged: failed, banned for an hour.
    publish_candidates(&provider, clk.now(), set());
    let deadline = clk.now().as_unix_nanos() + 50 * 60 * S;
    let (mut again, mut led) = (0, 0);
    while clk.now().as_unix_nanos() < deadline {
        let st = step(&mut s);
        again += usize::from(st.verification_group.is_some());
        led +=
            usize::from(matches!(st.purpose, Purpose::Bandit { lead: Some(k), .. } if k == ghost));
    }
    assert_eq!((again, led), (0, 0));
    let b = s.attention_status().bandit.unwrap();
    assert_eq!(
        (
            b.banned,
            b.counters.verifications_started,
            b.counters.verifications_failed
        ),
        (1, 1, 1)
    );
}

/// Three candidate windows; only the one at 421 MHz pays. Returns the steps of 3 simulated hours.
fn exploit_run() -> (Vec<ScheduleStep>, Scheduler<SyntheticClock>) {
    let (mut s, provider) = bandit_sched();
    let clk = s.clock().clone();
    publish_candidates(
        &provider,
        clk.now(),
        vec![
            candidate(1, 421.0, 50.0, 0.5, false),
            candidate(2, 449.0, 50.0, 0.5, false),
            candidate(3, 915.0, 50.0, 0.5, false),
        ],
    );
    let end = clk.now().as_unix_nanos() + 3 * 3600 * S;
    let mut steps = Vec::new();
    while clk.now().as_unix_nanos() < end {
        let st = step(&mut s);
        if let Purpose::Bandit { lead, .. } = st.purpose {
            let paid = lead == Some(candidate_key(1));
            s.record_outcome(&DwellOutcome {
                seq: st.seq,
                arm: s.arm_key_of(&st),
                dwell_s: st.duration_ns as f64 / 1e9,
                new_detections: 0,
                bursts: if paid { 20 } else { 0 },
                novelty_sum: 0.0,
                valid_decodes: 0,
                suspect_detections: 0,
            });
        }
        steps.push(st);
    }
    (steps, s)
}

#[test]
fn ucb_exploits_the_paying_window_inside_the_exploration_and_sweep_floors_deterministically() {
    let (steps, s) = exploit_run();
    let secs = |f: &dyn Fn(&ScheduleStep) -> bool| {
        steps
            .iter()
            .filter(|st| f(st))
            .map(|st| st.duration_ns)
            .sum::<i64>() as f64
            / 1e9
    };
    let lead = |k: i64| move |st: &ScheduleStep| matches!(st.purpose, Purpose::Bandit { lead: Some(l), .. } if l == candidate_key(k));
    let bandit = secs(&|st| matches!(st.purpose, Purpose::Bandit { .. }));
    let explore = secs(&|st| {
        matches!(
            st.purpose,
            Purpose::Bandit {
                kind: BanditKind::Explore,
                ..
            }
        )
    });
    let (a, b, c) = (secs(&lead(1)), secs(&lead(2)), secs(&lead(3)));
    let total = secs(&|_| true);
    let discovery = secs(&|st| st.purpose.is_discovery());
    assert!(
        a > 0.5 * bandit && a > 3.0 * b && a > 3.0 * c,
        "a {a} b {b} c {c} of {bandit}"
    );
    assert!(explore >= 0.13 * bandit, "explore {explore} of {bandit}");
    assert!(
        discovery >= 0.25 * total - 10.0,
        "discovery {discovery} of {total}"
    );
    // Sweep floor over every 10-minute window (step-boundary slack of one dwell).
    let t0 = steps[0].t_start.as_unix_nanos();
    for w in 0..17 {
        let (lo, hi) = (t0 + w * 600 * S, t0 + (w + 1) * 600 * S);
        let in_w = |f: &dyn Fn(&ScheduleStep) -> bool| {
            steps
                .iter()
                .filter(|st| f(st))
                .map(|st| {
                    (st.t_end().as_unix_nanos().min(hi) - st.t_start.as_unix_nanos().max(lo)).max(0)
                })
                .sum::<i64>() as f64
        };
        let (d, all) = (in_w(&|st| st.purpose.is_discovery()), in_w(&|_| true));
        assert!(
            d >= 0.25 * all - 10.0 * S as f64,
            "window {w}: {d} of {all}"
        );
    }
    // Starvation bound: every candidate window revisited within 30 minutes (+ one dwell).
    for k in 1..=3 {
        let visits: Vec<i64> = steps
            .iter()
            .filter(|st| lead(k)(st))
            .map(|st| st.t_start.as_unix_nanos())
            .collect();
        let max_gap = visits
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or(i64::MAX);
        assert!(
            max_gap <= 1830 * S,
            "candidate {k}: max gap {} s",
            max_gap / S
        );
    }
    let arms = s.arm_table();
    assert!(
        arms.iter().any(|a| a.exploration) && arms.iter().filter(|a| !a.exploration).count() == 3,
        "{arms:?}"
    );
    let best = arms
        .iter()
        .filter(|a| a.lead == Some(candidate_key(1)))
        .map(|a| a.mean_reward)
        .next()
        .unwrap();
    assert!(
        arms.iter()
            .filter(|a| a.lead.is_some())
            .all(|a| a.mean_reward <= best)
    );
    let status = s.attention_status();
    assert_eq!(status.sweep_floor, Some(0.25));
    assert!(status.bandit.unwrap().counters.floor_deferrals > 0);
    // Determinism: the same inputs give the same steps.
    assert_eq!(exploit_run().0, steps);
}

#[test]
fn low_power_raises_the_sweep_floor() {
    let (mut s, provider) = bandit_sched();
    let clk = s.clock().clone();
    publish_candidates(
        &provider,
        clk.now(),
        vec![candidate(1, 433.92, 50.0, 0.9, false)],
    );
    s.set_low_power(true);
    let end = clk.now().as_unix_nanos() + 3600 * S;
    let (mut disc, mut all) = (0i64, 0i64);
    while clk.now().as_unix_nanos() < end {
        let st = step(&mut s);
        all += st.duration_ns;
        if st.purpose.is_discovery() {
            disc += st.duration_ns;
        }
    }
    let status = s.attention_status();
    assert!(status.low_power && status.sweep_floor == Some(0.5));
    assert!(
        disc as f64 >= 0.5 * all as f64 - 10.0 * S as f64,
        "{disc} of {all}"
    );
}

#[test]
fn region_poi_from_the_scheduler_windows_matches_the_periodic_formula() {
    // Two 15 MHz regions, one 50 ms hop each: T_d 50 ms, T_R 100 ms.
    let p = plan(
        "poi",
        1,
        vec![
            region(900.0, 915.0, 1.0, None),
            region(2400.0, 2415.0, 1.0, None),
        ],
        ScanPolicy::SweepOnly,
        vec![],
        json!({}),
    );
    let mut s = hackrf(&p);
    assert_eq!(s.plan().hops.len(), 2);
    let t0 = s.clock().now();
    let steps = run(&mut s, 2_000);
    let span = TimeRange::new(
        t0.saturating_add_nanos(10 * S),
        Timestamp::from_unix_nanos(steps.last().unwrap().t_start.as_unix_nanos() - 10 * S),
    );
    let taus = [0.005, 0.02];
    let r = s.region_poi(FreqRange::new(901e6, 914e6), span, &taus, Some(2.0));
    assert_eq!(r.observed_cells, r.cells);
    for (e, tau) in r.poi.iter().zip(taus) {
        let nominal = nominal_poi(tau, 0.05, 0.1);
        assert!(
            (e.p_poi - nominal).abs() < 1e-3,
            "tau {tau}: {} vs {nominal}",
            e.p_poi
        );
        assert!(e.p_at_least_one.unwrap() > 0.99);
    }
    assert!((r.mean_revisit_s.unwrap() - 0.1).abs() < 1e-6);
    assert!(r.gaps.is_empty(), "{:?}", r.gaps);
}
