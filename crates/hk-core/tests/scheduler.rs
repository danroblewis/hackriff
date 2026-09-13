//! T-009 scheduler behaviour: SPACE-050 coverage within revisit targets, AWARE-042 sweep/dwell
//! interleave, POI verification feeding the T-006 trust tests, preemption, capability limits,
//! plan transitions and the TX / accessory placeholders.

mod sched_common;

use hk_core::SourceCapabilities;
use hk_core::scheduler::{
    AnchoredClock, CaptureTrust, GainSlot, MemorySurveyLog, PlanError, PlanWarning, Poi, Purpose,
    RetunePlan, RetuneSkip, ScheduleStep, Scheduler, SchedulerConfig, SchedulerError,
    SyntheticClock, TrustEvaluator, TxSlotRequest, UserIntent, Verification, VerifyError, rf_path,
};
use hk_detect::{
    CaptureEmitter, CaptureResult, EdgeRule, GainState, GainStepConfig, GainStepResult,
    GainStepVerdict, Geometry, IntegratedSnapshot, RateChangeConfig, RateChangeLabel,
    RateChangeResult, RetuneConfig, RetuneLabel, RetuneResult,
};
use hk_model::{
    FreqRange, Repository, ScanPolicy, Schedule, SurveyState, SurveySummary, Timestamp,
};
use sched_common::*;
use serde_json::{Value, json};

fn poi(key: u64, center_hz: f64, bandwidth_hz: f64, interestingness: f64) -> Poi {
    Poi {
        key,
        center_hz,
        bandwidth_hz,
        interestingness,
        burst_interval_ns: None,
        verify: false,
    }
}

/// Runs until the synthetic clock passes `until_ns` after T0.
fn run_until(
    s: &mut Scheduler<hk_core::scheduler::SyntheticClock>,
    until_ns: i64,
) -> Vec<ScheduleStep> {
    let mut out = Vec::new();
    while s.clock().now_ns() < T0_NS + until_ns {
        s.run_synthetic(1, &mut out);
    }
    out
}

trait NowNs {
    fn now_ns(&self) -> i64;
}

impl NowNs for hk_core::scheduler::SyntheticClock {
    fn now_ns(&self) -> i64 {
        use hk_core::scheduler::Clock;
        self.now().as_unix_nanos()
    }
}

#[test]
fn space_050_wide_survey_covers_every_region_within_its_revisit_target() {
    // SPACE-050: a 1 MHz–6 GHz noise-floor survey in three regions with 30 s revisit targets,
    // with POI dwells competing for the window.
    let p = plan(
        "SPACE-050 wide survey",
        1,
        vec![
            region(1.0, 30.0, 2.0, Some(30.0)),
            region(30.0, 1000.0, 1.0, Some(30.0)),
            region(1000.0, 6000.0, 1.0, Some(30.0)),
        ],
        ScanPolicy::SweepThenDwell,
        vec![],
        json!({ "scheduler": { "sweep_step_s": 0.04, "sweeps_per_cycle": 20, "dwells_per_cycle": 1 } }),
    );
    let mut s = hackrf(&p);
    let compiled = s.plan().clone();
    assert!(
        !compiled.warnings.iter().any(|w| matches!(
            w,
            PlanWarning::RevisitUnachievable { .. } | PlanWarning::DwellsCappedAtMinimum { .. }
        )),
        "{:?}",
        compiled.warnings
    );
    assert!(
        compiled.revisit_bound_ns <= 30 * S,
        "{}",
        compiled.revisit_bound_ns
    );
    for (key, f) in [(1, 7.1e6), (2, 145.5e6), (3, 1090e6), (4, 2437e6)] {
        s.offer_poi(Poi {
            burst_interval_ns: Some(60 * S),
            ..poi(key, f, 100e3, 1.0)
        })
        .unwrap();
    }
    let steps = run_until(&mut s, 90 * S);
    let end = s.clock().now_ns();

    let dwells: Vec<_> = steps
        .iter()
        .filter(|st| matches!(st.purpose, Purpose::Dwell { .. }))
        .collect();
    assert!(dwells.len() > 10, "POI dwells ran: {}", dwells.len());
    assert!(
        dwells
            .iter()
            .all(|d| d.duration_ns <= compiled.dwell_cap_ns)
    );

    for (ri, r) in compiled.regions.iter().enumerate() {
        let target = r.revisit_ns.unwrap();
        let mut hops: Vec<_> = compiled
            .hops
            .iter()
            .enumerate()
            .filter(|(_, h)| {
                h.covers.lo_hz < r.requested.hi_hz && h.covers.hi_hz > r.requested.lo_hz
            })
            .collect();
        hops.sort_by(|a, b| a.1.covers.lo_hz.total_cmp(&b.1.covers.lo_hz));
        // The hops tile the whole region, without gaps.
        assert!(
            hops[0].1.covers.lo_hz <= r.requested.lo_hz + 1.0,
            "region {ri} start"
        );
        assert!(
            hops.last().unwrap().1.covers.hi_hz >= r.requested.hi_hz - 1.0,
            "region {ri} end"
        );
        for w in hops.windows(2) {
            assert!(
                (w[0].1.covers.hi_hz - w[1].1.covers.lo_hz).abs() < 1.0,
                "gap in region {ri}"
            );
        }
        // Every hop is visited within the target from the start, between visits and to the end.
        for (index, _) in &hops {
            let mut last_done = T0_NS;
            let mut visits = 0;
            for st in steps
                .iter()
                .filter(|st| st.purpose == Purpose::Sweep { hop: *index as u32 })
            {
                let done = st.t_end().as_unix_nanos();
                assert!(
                    done - last_done <= target,
                    "region {ri} hop {index}: revisit {} ns",
                    done - last_done
                );
                last_done = done;
                visits += 1;
            }
            assert!(
                visits >= 3,
                "region {ri} hop {index}: {visits} visits in 90 s"
            );
            assert!(
                end - last_done <= target,
                "region {ri} hop {index}: stale at the end"
            );
        }
    }
}

#[test]
fn aware_042_poi_dwells_interleave_with_discovery_at_the_configured_ratio() {
    for (sweeps, dwells) in [(3usize, 1usize), (2, 2), (5, 1)] {
        let p = plan(
            "AWARE-042",
            1,
            vec![region(902.0, 928.0, 1.0, None)],
            ScanPolicy::SweepThenDwell,
            vec![],
            json!({ "scheduler": { "sweeps_per_cycle": sweeps, "dwells_per_cycle": dwells } }),
        );
        let mut s = hackrf(&p);
        let before = run(&mut s, 10);
        assert!(
            before.iter().all(|st| st.purpose.is_discovery()),
            "no POIs: discovery only"
        );

        let bursty = Poi {
            burst_interval_ns: Some(2 * S),
            ..poi(1, 915.0e6, 500e3, 1.0)
        };
        let wide = poi(2, 906.4e6, 1.2e6, 3.0);
        s.offer_poi(bursty).unwrap();
        s.offer_poi(wide).unwrap();
        let steps = run(&mut s, 400);

        let kinds: String = steps
            .iter()
            .map(|st| if st.purpose.is_discovery() { 'S' } else { 'D' })
            .collect();
        let first_d = kinds.find('D').expect("dwells start");
        assert!(first_d <= sweeps, "{kinds}");
        let cycle = "S".repeat(sweeps) + &"D".repeat(dwells);
        let tail = &kinds[first_d + dwells..];
        assert_eq!(&kinds[first_d..first_d + dwells], &"D".repeat(dwells));
        for chunk in tail.as_bytes().chunks(sweeps + dwells) {
            assert_eq!(
                chunk,
                &cycle.as_bytes()[..chunk.len()],
                "ratio {sweeps}:{dwells}: {kinds}"
            );
        }

        // POI-sized dwells: fast enough, emitter off DC and inside the usable span.
        let (mut n1, mut n2) = (0usize, 0usize);
        for st in steps.iter().filter(|st| !st.purpose.is_discovery()) {
            let (target, expect_ns) = match st.purpose {
                Purpose::Dwell { poi: 1 } => (bursty, 6 * S),
                Purpose::Dwell { poi: 2 } => (wide, 2 * S),
                other => panic!("unexpected {other:?}"),
            };
            if target.key == 1 {
                n1 += 1
            } else {
                n2 += 1
            }
            let usable = st.rate_hz * 0.75;
            let offset = (target.center_hz - st.center_hz).abs();
            assert!(st.rate_hz >= 8e6);
            assert!(
                offset - target.bandwidth_hz / 2.0 > 0.0,
                "emitter clears DC"
            );
            assert!(
                offset + target.bandwidth_hz / 2.0 <= usable / 2.0,
                "emitter in span"
            );
            assert_eq!(
                st.duration_ns, expect_ns,
                "3 burst intervals, else the default"
            );
        }
        // Weighted round robin: interestingness 3 vs 1.
        let share = n2 as f64 / n1 as f64;
        assert!((share - 3.0).abs() < 0.5, "dwell share {n2}:{n1}");
    }
}

// ---- POI verification feeding hk-detect trust tests ----

const BINS: usize = 4096;

/// A capture and the `seq` of the step it was made in.
struct Cap(CaptureResult, u64);

impl CaptureTrust for Cap {
    fn clipped(&self) -> bool {
        self.0.clipped
    }
}

/// A synthetic integrated capture of a fixed scene (absolute frequencies), as the step's window
/// sees it: floor and signals scale with the LNA step (analog-noise-limited, linear).
fn capture(step: &ScheduleStep, clipped: bool) -> Cap {
    let scene = [
        (912.2e6, 150e3, 20.0),
        (913.0e6, 150e3, 20.0),
        (914.4e6, 150e3, 20.0),
        (915.2e6, 200e3, 15.0),
    ];
    let bw = step.baseband_filter_hz.unwrap_or(0.0);
    let geometry = Geometry::new(step.center_hz, step.rate_hz, BINS, bw, &EdgeRule::default());
    let floor = 1e-12 * 10f64.powf((step.gains.lna_db - 24.0) / 10.0);
    let mut psd = vec![floor; BINS];
    let mut emitters = Vec::new();
    for (f, w, snr_db) in scene {
        if (f - step.center_hz).abs() + w / 2.0 > geometry.usable_half_hz {
            continue;
        }
        let lo = geometry.bin_at_or_above(f - w / 2.0);
        let hi = geometry.bin_at_or_above(f + w / 2.0).max(lo + 1);
        for p in &mut psd[lo..hi] {
            *p += floor * 10f64.powf(snr_db / 10.0);
        }
        emitters.push(CaptureEmitter {
            f_lo_hz: f - w / 2.0,
            f_hi_hz: f + w / 2.0,
            f_center_hz: f,
            bandwidth_hz: w,
            peak_excess_dbfs: 10.0 * ((psd[lo] - floor) * geometry.bin_width_hz).log10(),
            spur: false,
            dc: false,
            image: false,
            edge: false,
        });
    }
    Cap(
        CaptureResult {
            center_hz: step.center_hz,
            gain: GainState {
                lna_db: step.gains.lna_db,
                vga_db: step.gains.vga_db,
                amp_on: step.gains.amp_on,
            },
            quantisation_limited: false,
            clipped,
            spectrum: IntegratedSnapshot {
                geometry,
                span_s: step.duration_ns as f64 / 1e9,
                mean_psd: psd.clone(),
                mean_floor: vec![floor; BINS],
                block_psd: vec![psd; 4],
            },
            emitters,
        },
        step.seq,
    )
}

#[derive(Default)]
struct Evaluator {
    gain_steps: Vec<(u8, GainStepResult)>,
    retunes: Vec<(f64, RetuneResult)>,
    rate_changes: Vec<RateChangeResult>,
    /// `seq` of the base capture of every retune and rate-change comparison.
    bases: Vec<u64>,
}

impl TrustEvaluator<Cap> for Evaluator {
    fn gain_step(&mut self, _poi: u64, pair: u8, lower: &Cap, higher: &Cap) {
        let r = hk_detect::gain_step(&lower.0, &higher.0, &GainStepConfig::default());
        self.gain_steps.push((pair, r));
    }

    fn retune(&mut self, _poi: u64, base: &Cap, moved: &Cap, delta_hz: f64) {
        let r = hk_detect::retune(&base.0, &moved.0, &RetuneConfig::default());
        self.retunes.push((delta_hz, r));
        self.bases.push(base.1);
    }

    fn rate_change(&mut self, _poi: u64, base: &Cap, changed: &Cap, base_rate: f64, rate: f64) {
        let r = hk_detect::rate_change(
            &base.0,
            &changed.0,
            base_rate,
            rate,
            &RateChangeConfig::default(),
        );
        self.rate_changes.push(r);
        self.bases.push(base.1);
    }
}

/// The verification group of POI `key`: its first contiguous run of trust-test steps.
fn group_of(steps: &[ScheduleStep], key: u64) -> Vec<ScheduleStep> {
    let start = steps
        .iter()
        .position(|st| st.purpose.poi() == Some(key))
        .expect("the POI is served");
    steps[start..]
        .iter()
        .take_while(|st| st.purpose.is_trust_test())
        .copied()
        .collect()
}

#[test]
fn poi_verification_interleaves_gain_steps_and_retunes_and_skips_clipped_pairs() {
    let p = plan(
        "verify",
        1,
        vec![region(902.0, 928.0, 1.0, None)],
        ScanPolicy::SweepThenDwell,
        vec![gain(900.0, 930.0, 24.0, 20.0, false, None)],
        json!({ "scheduler": { "sweeps_per_cycle": 2, "rate_change": true } }),
    );
    let mut s = hackrf(&p);
    s.offer_poi(Poi {
        verify: true,
        ..poi(7, 915.2e6, 200e3, 1.0)
    })
    .unwrap();
    let steps = run(&mut s, 30);
    let start = steps
        .iter()
        .position(|st| st.purpose.poi() == Some(7))
        .unwrap();
    let group: Vec<ScheduleStep> = steps[start..]
        .iter()
        .take_while(|st| st.purpose.is_trust_test())
        .copied()
        .collect();
    let dwell = steps[start + group.len()..]
        .iter()
        .find(|st| st.purpose == Purpose::Dwell { poi: 7 })
        .copied()
        .expect("a plain dwell after verification");
    assert_eq!(s.is_verified(7), Some(true));

    // S4 rule 6: A/B × 3 at 0.5 s, B = LNA + 8 dB, same window as the POI dwell.
    assert_eq!(group.len(), 9, "{group:#?}");
    for (i, st) in group[..6].iter().enumerate() {
        let slot = if i % 2 == 0 { GainSlot::A } else { GainSlot::B };
        assert_eq!(
            st.purpose,
            Purpose::GainStep {
                poi: 7,
                pair: (i / 2) as u8,
                slot
            }
        );
        assert_eq!(
            (st.center_hz, st.rate_hz, st.duration_ns),
            (dwell.center_hz, dwell.rate_hz, S / 2)
        );
        let lna = if slot == GainSlot::A { 24.0 } else { 32.0 };
        assert_eq!(
            (st.gains.lna_db, st.gains.vga_db, st.gains.amp_on),
            (lna, 20.0, false)
        );
    }
    // S4 rule 7: ±1 MHz at the A gains; then the clock-harmonic rate change.
    assert_eq!(
        group[6].purpose,
        Purpose::Retune {
            poi: 7,
            delta_hz: 1e6
        }
    );
    assert_eq!(group[6].center_hz, dwell.center_hz + 1e6);
    assert_eq!(
        group[7].purpose,
        Purpose::Retune {
            poi: 7,
            delta_hz: -1e6
        }
    );
    assert_eq!(group[7].center_hz, dwell.center_hz - 1e6);
    assert_eq!(
        group[8].purpose,
        Purpose::RateChange {
            poi: 7,
            base_rate_hz: dwell.rate_hz
        }
    );
    assert_ne!(group[8].rate_hz, dwell.rate_hz);
    assert_eq!(group[8].center_hz, dwell.center_hz);
    for w in group.windows(2) {
        assert_eq!(w[1].t_start, w[0].t_end(), "one contiguous dwell");
    }

    // Clean captures: every pair and both retunes reach the T-006 trust functions.
    let mut clean = Verification::new(7, 3);
    for st in &group {
        clean.record(st, capture(st, false)).unwrap();
    }
    assert!(clean.record(&steps[0], capture(&steps[0], false)).is_err());
    let mut eval = Evaluator::default();
    let report = clean.evaluate(&mut eval);
    assert_eq!(
        (report.gain_pairs_run, report.gain_pairs_skipped_clipped),
        (3, 0)
    );
    assert_eq!((report.retunes_run, report.rate_changes_run), (2, 1));
    for (_, r) in &eval.gain_steps {
        assert_eq!(r.skipped, None);
        assert!((r.nominal_db - 8.0).abs() < 1e-9, "lower → higher ordering");
        assert!(r.anchors >= 3 && (r.g_lin_db - 8.0).abs() < 0.5, "{r:?}");
        assert!(
            r.rows
                .iter()
                .all(|row| row.verdict == GainStepVerdict::Linear),
            "{r:?}"
        );
    }
    for (delta, r) in &eval.retunes {
        assert_eq!(r.delta_hz, *delta);
        assert!(
            !r.rows.is_empty() && r.rows.iter().all(|row| row.label == RetuneLabel::Stays),
            "{r:?}"
        );
    }
    // The real scene stays at absolute frequency across the rate change (hk_detect rate_change).
    let r = &eval.rate_changes[0];
    assert!(
        r.skipped.is_none()
            && !r.rows.is_empty()
            && r.rows.iter().all(|row| row.label == RateChangeLabel::Stays),
        "{r:?}"
    );
    assert!(
        group
            .iter()
            .all(|st| st.verification_group == Some(group[0].seq))
    );
    assert_eq!(dwell.verification_group, None);

    // A clipped block: that pair never reaches gain-step inference.
    let mut clipped = Verification::new(7, 3);
    for st in &group {
        let clip = st.purpose
            == Purpose::GainStep {
                poi: 7,
                pair: 1,
                slot: GainSlot::B,
            };
        clipped.record(st, capture(st, clip)).unwrap();
    }
    let mut eval = Evaluator::default();
    let report = clipped.evaluate(&mut eval);
    assert_eq!(
        (report.gain_pairs_run, report.gain_pairs_skipped_clipped),
        (2, 1)
    );
    assert_eq!(
        eval.gain_steps
            .iter()
            .map(|(pair, _)| *pair)
            .collect::<Vec<_>>(),
        vec![0, 2]
    );
    assert_eq!(
        report.retunes_run, 2,
        "retune comparisons do not depend on clipping"
    );
}

#[test]
fn user_intent_preempts_and_the_cut_slot_is_revisited() {
    let p = plan(
        "preempt",
        1,
        vec![region(88.0, 148.0, 1.0, None)],
        ScanPolicy::SweepThenDwell,
        vec![],
        Value::Null,
    );
    let mut s = hackrf(&p);
    let clk = s.clock().clone();
    run(&mut s, 2);
    let cut = s.next_step();
    assert_eq!(cut.purpose, Purpose::Sweep { hop: 2 });
    clk.advance_ns(20_000_000);
    let intent = UserIntent {
        id: 42,
        center_hz: 101.1e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: Some(2_500_000_000),
    };
    s.preempt(intent).unwrap();
    let held = run(&mut s, 3);
    assert!(
        held.iter()
            .all(|st| st.purpose == Purpose::UserIntent { intent: 42 }
                && st.center_hz == 101.1e6
                && st.rate_hz == 10e6
                && st.baseband_filter_hz == Some(7e6))
    );
    assert_eq!(
        held.iter().map(|st| st.duration_ns).collect::<Vec<_>>(),
        vec![S, S, S / 2]
    );
    let resumed = s.next_step();
    assert_eq!(
        resumed.purpose,
        Purpose::Sweep { hop: 2 },
        "the cut hop is re-visited"
    );
    assert_eq!(
        resumed.t_start.as_unix_nanos(),
        cut.t_start.as_unix_nanos() + 20_000_000 + 2_500_000_000
    );
    assert_eq!(s.stats().truncated_slots, 1);

    // An open-ended intent holds until released.
    clk.advance_ns(resumed.duration_ns);
    s.preempt(UserIntent {
        id: 43,
        duration_ns: None,
        ..intent
    })
    .unwrap();
    let held = run(&mut s, 5);
    assert!(
        held.iter()
            .all(|st| st.purpose == Purpose::UserIntent { intent: 43 } && st.duration_ns == S)
    );
    assert!(s.release_intent());
    assert!(!s.release_intent());
    assert!(s.next_step().purpose.is_discovery());

    for bad in [
        UserIntent {
            center_hz: 10e9,
            ..intent
        },
        UserIntent {
            rate_hz: 40e6,
            ..intent
        },
        UserIntent {
            duration_ns: Some(0),
            ..intent
        },
    ] {
        assert!(matches!(
            s.preempt(bad),
            Err(SchedulerError::OutOfCapability { .. })
        ));
    }

    // Preempting a verification group mid-way restarts it from its first stage afterwards.
    let p = plan(
        "preempt verify",
        1,
        vec![region(902.0, 928.0, 1.0, None)],
        ScanPolicy::SweepThenDwell,
        vec![],
        json!({ "scheduler": { "sweeps_per_cycle": 1 } }),
    );
    let mut s = hackrf(&p);
    let clk = s.clock().clone();
    s.offer_poi(Poi {
        verify: true,
        ..poi(5, 915e6, 100e3, 1.0)
    })
    .unwrap();
    let first = run(&mut s, 2);
    assert_eq!(
        first[1].purpose,
        Purpose::GainStep {
            poi: 5,
            pair: 0,
            slot: GainSlot::A
        }
    );
    let b0 = s.next_step();
    assert_eq!(
        b0.purpose,
        Purpose::GainStep {
            poi: 5,
            pair: 0,
            slot: GainSlot::B
        }
    );
    clk.advance_ns(100_000_000);
    s.preempt(UserIntent {
        duration_ns: Some(S),
        ..intent
    })
    .unwrap();
    let after = run(&mut s, 3);
    assert_eq!(after[0].purpose, Purpose::UserIntent { intent: 42 });
    assert_eq!(
        after[1].purpose,
        Purpose::GainStep {
            poi: 5,
            pair: 0,
            slot: GainSlot::A
        }
    );
    assert_eq!(
        after[2].purpose,
        Purpose::GainStep {
            poi: 5,
            pair: 0,
            slot: GainSlot::B
        }
    );
    assert_eq!(s.is_verified(5), Some(false));
}

#[test]
fn steps_respect_source_capabilities() {
    let caps = SourceCapabilities::hackrf_one();
    let p = plan(
        "limits",
        1,
        vec![
            region(0.1, 40.0, 1.0, None),
            region(5990.0, 7000.0, 1.0, None),
            region(2100.0, 2800.0, 1.0, None),
        ],
        ScanPolicy::SweepThenDwell,
        vec![],
        json!({ "scheduler": { "usable_fraction": 1.0, "rate_change": true } }),
    );
    let mut s = hackrf(&p);
    let compiled = s.plan().clone();
    assert!(
        compiled
            .warnings
            .contains(&PlanWarning::ClippedToCapabilities { region: 0 })
    );
    assert!(
        compiled
            .warnings
            .contains(&PlanWarning::ClippedToCapabilities { region: 1 })
    );
    // RF-path boundaries come from the source (firmware tuning.c values), not the config.
    assert!(s.config().rf_path_boundaries_hz.is_empty());
    assert_eq!(caps.rf_path_boundaries_hz, vec![2170e6, 2740e6]);
    let bounds = caps.rf_path_boundaries_hz.clone();
    for h in &compiled.hops {
        assert!(
            h.covers.width_hz() <= 20e6 + 1.0
                && h.covers.width_hz() <= compiled.usable_span_hz + 1.0
        );
        assert!(h.covers.lo_hz >= h.center_hz - compiled.usable_span_hz / 2.0 - 1.0);
        assert!(h.covers.hi_hz <= h.center_hz + compiled.usable_span_hz / 2.0 + 1.0);
        assert!(h.covers.lo_hz >= 1e6 && h.covers.hi_hz <= 6e9);
        let (a, b) = (
            rf_path(&bounds, h.covers.lo_hz),
            rf_path(&bounds, h.covers.hi_hz - 1.0),
        );
        assert_eq!(
            (a, b),
            (h.rf_path, h.rf_path),
            "no hop straddles an RF-path switch: {h:?}"
        );
    }
    for (key, f, bw) in [
        (1, 1.2e6, 50e3),
        (2, 5999.5e6, 100e3),
        (3, 2400e6, 7e6),
        (4, 100e6, 30e6),
    ] {
        s.offer_poi(Poi {
            verify: key % 2 == 0,
            ..poi(key, f, bw, 1.0)
        })
        .unwrap();
    }
    assert!(matches!(
        s.offer_poi(poi(9, 10e9, 1e3, 1.0)),
        Err(SchedulerError::OutOfCapability { .. })
    ));
    assert!(matches!(
        s.offer_poi(poi(9, 100e6, f64::NAN, 1.0)),
        Err(SchedulerError::InvalidPoi(_))
    ));
    let filters = caps.baseband_filter.clone().unwrap();
    for st in run(&mut s, 800) {
        assert!(caps.supports_frequency(st.center_hz), "{st:?}");
        assert!(caps.sample_rates.supports(st.rate_hz), "{st:?}");
        assert!(filters.supports(st.baseband_filter_hz.unwrap()), "{st:?}");
        assert!(hk_core::scheduler::check_gains(&caps, &st.gains).is_ok());
        assert_eq!(st.rf_path, rf_path(&bounds, st.center_hz));
        assert!(st.duration_ns > 0);
    }

    let with = |extra: Value| {
        plan(
            "bad",
            1,
            vec![region(88.0, 108.0, 1.0, None)],
            ScanPolicy::SweepOnly,
            vec![],
            json!({ "scheduler": extra }),
        )
    };
    for extra in [
        json!({ "sweep_rate_hz": 40e6 }),
        json!({ "usable_fraction": 1.5 }),
        json!({ "sweeps_per_cycle": 0 }),
    ] {
        let p = with(extra.clone());
        let r = Scheduler::new(&p, SchedulerConfig::from_plan(&p).unwrap(), &caps, clock());
        assert!(
            matches!(
                r.err(),
                Some(SchedulerError::Plan(PlanError::InvalidConfig(_)))
            ),
            "{extra}"
        );
    }
    let lna44 = plan(
        "bad gain",
        1,
        vec![region(88.0, 108.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![gain(80.0, 110.0, 44.0, 20.0, false, None)],
        Value::Null,
    );
    let r = Scheduler::new(&lna44, SchedulerConfig::default(), &caps, clock());
    assert!(matches!(
        r.err(),
        Some(SchedulerError::Plan(PlanError::InvalidGain {
            index: Some(0),
            ..
        }))
    ));
    let outside = plan(
        "outside",
        1,
        vec![region(7000.0, 8000.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    let r = Scheduler::new(&outside, SchedulerConfig::default(), &caps, clock());
    assert!(matches!(
        r.err(),
        Some(SchedulerError::Plan(PlanError::NothingCoverable))
    ));
}

#[test]
fn plan_version_change_mid_survey_closes_and_reopens_the_survey() {
    let mut repo = Repository::open_in_memory().unwrap();
    let v1 = plan(
        "FM survey",
        1,
        vec![region(88.0, 108.0, 1.0, Some(60.0))],
        ScanPolicy::SweepThenDwell,
        vec![],
        Value::Null,
    );
    repo.insert_scan_plan(&v1).unwrap();
    let mut s = hackrf(&v1);
    let first = s.open_survey(&mut repo, "hackrf:t-009").unwrap();
    assert!(matches!(
        s.open_survey(&mut repo, "hackrf:t-009"),
        Err(SchedulerError::SurveyAlreadyOpen)
    ));
    assert!(run(&mut s, 5).iter().all(|st| st.plan_version == 1));

    let mut v2 = v1.clone();
    v2.version = 2;
    v2.regions.push(region(144.0, 148.0, 2.0, None));
    repo.insert_scan_plan(&v2).unwrap();
    let summary = SurveySummary {
        sweep_frames: 5,
        ..Default::default()
    };
    let changed_at = hk_core::scheduler::Clock::now(s.clock());
    let second = s
        .update_plan(
            &v2,
            SchedulerConfig::from_plan(&v2).unwrap(),
            &mut repo,
            &summary,
        )
        .unwrap()
        .expect("a new survey");
    assert_ne!(first, second);
    assert_eq!(s.survey_id(), Some(second));

    let old = repo.survey(first).unwrap();
    assert_eq!(
        (old.state, old.plan_version, old.t_end),
        (SurveyState::Closed, 1, Some(changed_at))
    );
    assert_eq!(old.summary, Some(summary.clone()));
    let new = repo.survey(second).unwrap();
    assert_eq!(
        (new.state, new.plan_version, new.t_start),
        (SurveyState::Open, 2, changed_at)
    );
    assert_eq!(new.device_id, "hackrf:t-009");

    let after = run(&mut s, 5);
    assert!(after.iter().all(|st| st.plan_version == 2));
    // v1 had visited 88–98 MHz and was due at 98–108 MHz: the pass resumes at the equivalent
    // hop of v2, then wraps to the new priority-2 region that leads the pass.
    assert_eq!(after[0].purpose, Purpose::Sweep { hop: 2 });
    assert!((s.plan().hops[2].covers.lo_hz - 98e6).abs() < 1.0);
    assert_eq!(after[1].purpose, Purpose::Sweep { hop: 0 });
    assert!(
        s.plan().hops[0].covers.lo_hz >= 144e6,
        "the new priority-2 region leads the pass"
    );

    let stale = s.update_plan(&v1, SchedulerConfig::default(), &mut repo, &summary);
    assert!(matches!(
        stale,
        Err(SchedulerError::Plan(PlanError::StaleVersion {
            version: 1,
            running: 2,
            ..
        }))
    ));
    assert_eq!(s.next_step().plan_version, 2);

    s.close_survey(&mut repo, SurveyState::Closed, &SurveySummary::default())
        .unwrap();
    assert_eq!(repo.survey(second).unwrap().state, SurveyState::Closed);
    assert!(matches!(
        s.close_survey(&mut repo, SurveyState::Closed, &SurveySummary::default()),
        Err(SchedulerError::SurveyNotOpen)
    ));
}

#[test]
fn empty_and_malformed_plans_fail_cleanly() {
    let caps = SourceCapabilities::hackrf_one();
    let new = |p: &hk_model::ScanPlan| {
        Scheduler::new(p, SchedulerConfig::default(), &caps, clock()).err()
    };
    let empty = plan(
        "empty",
        1,
        vec![],
        ScanPolicy::SweepThenDwell,
        vec![],
        Value::Null,
    );
    assert!(matches!(
        new(&empty),
        Some(SchedulerError::Plan(PlanError::Empty))
    ));
    let inverted = plan(
        "inverted",
        1,
        vec![region(108.0, 88.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    assert!(matches!(
        new(&inverted),
        Some(SchedulerError::Plan(PlanError::InvalidRegion {
            index: 0,
            ..
        }))
    ));
    let nan = plan(
        "nan",
        1,
        vec![region(88.0, 108.0, f64::NAN, None)],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    assert!(matches!(
        new(&nan),
        Some(SchedulerError::Plan(PlanError::InvalidRegion { .. }))
    ));
    let mut cron = plan(
        "cron",
        1,
        vec![region(88.0, 108.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    cron.schedule = Schedule::Cron {
        expr: "0 * * * *".into(),
    };
    assert!(matches!(
        new(&cron),
        Some(SchedulerError::Plan(PlanError::UnsupportedSchedule("cron")))
    ));
    let typo = plan(
        "typo",
        1,
        vec![region(88.0, 108.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        json!({ "scheduler": { "sweep_steps": 3 } }),
    );
    assert!(matches!(
        SchedulerConfig::from_plan(&typo),
        Err(PlanError::InvalidConfig(_))
    ));
    let policies = plan(
        "policies",
        1,
        vec![region(88.0, 108.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        json!({ "scheduler": { "region_policy": [null, "dwell-only"] } }),
    );
    let r = Scheduler::new(
        &policies,
        SchedulerConfig::from_plan(&policies).unwrap(),
        &caps,
        clock(),
    );
    assert!(matches!(
        r.err(),
        Some(SchedulerError::Plan(PlanError::InvalidConfig(_)))
    ));

    // A failed update keeps the running plan and survey untouched.
    let good = plan(
        "good",
        1,
        vec![region(88.0, 108.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    let mut s = hackrf(&good);
    let mut log = MemorySurveyLog::default();
    let id = s.open_survey(&mut log, "synthetic:t-009").unwrap();
    let mut bad = empty.clone();
    bad.version = 2;
    let r = s.update_plan(
        &bad,
        SchedulerConfig::default(),
        &mut log,
        &SurveySummary::default(),
    );
    assert!(matches!(r, Err(SchedulerError::Plan(PlanError::Empty))));
    assert_eq!(log.events.len(), 1);
    assert_eq!(s.survey_id(), Some(id));
    assert_eq!(s.next_step().plan_version, 1);
    assert!(matches!(
        s.close_survey(&mut log, SurveyState::Open, &SurveySummary::default()),
        Err(SchedulerError::InvalidSurveyState)
    ));
}

#[test]
fn overlapping_regions_are_merged_and_visited_once_per_pass() {
    let p = plan(
        "overlap",
        1,
        vec![
            region(100.0, 120.0, 1.0, None),
            region(110.0, 140.0, 5.0, None),
            region(100.0, 120.0, 2.0, None),
            region(139.0, 141.0, 1.0, None),
        ],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    let mut s = hackrf(&p);
    let compiled = s.plan().clone();
    let mut covers: Vec<FreqRange> = compiled.hops.iter().map(|h| h.covers).collect();
    covers.sort_by(|a, b| a.lo_hz.total_cmp(&b.lo_hz));
    assert!(
        (covers[0].lo_hz - 100e6).abs() < 1.0 && (covers.last().unwrap().hi_hz - 141e6).abs() < 1.0
    );
    for w in covers.windows(2) {
        assert!(
            (w[0].hi_hz - w[1].lo_hz).abs() < 1.0,
            "tiles without gaps or overlap"
        );
    }
    let total: f64 = covers.iter().map(FreqRange::width_hz).sum();
    assert!(
        (total - 41e6).abs() < 1.0,
        "no frequency is covered twice: {total}"
    );
    for h in &compiled.hops {
        let best = p
            .regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.freq.lo_hz < h.covers.hi_hz && r.freq.hi_hz > h.covers.lo_hz)
            .max_by(|a, b| a.1.priority.total_cmp(&b.1.priority).then(b.0.cmp(&a.0)))
            .unwrap();
        assert_eq!(h.region as usize, best.0);
    }
    let pass = run(&mut s, compiled.hops.len());
    let mut visited: Vec<u32> = pass
        .iter()
        .map(|st| match st.purpose {
            Purpose::Sweep { hop } => hop,
            other => panic!("{other:?}"),
        })
        .collect();
    visited.sort_unstable();
    assert_eq!(visited, (0..compiled.hops.len() as u32).collect::<Vec<_>>());
    assert_eq!(s.passes_completed(), 1);
}

#[test]
fn tx_slots_are_gated_and_accessory_bands_are_flagged() {
    let p = plan(
        "accessory",
        1,
        vec![
            region(80.0, 110.0, 1.0, None),
            region(400.0, 410.0, 1.0, None),
        ],
        ScanPolicy::SweepThenDwell,
        vec![gain(86.0, 110.0, 16.0, 20.0, false, Some("fm-notch"))],
        Value::Null,
    );
    let mut s = hackrf(&p);
    assert!(
        s.plan()
            .warnings
            .contains(&PlanWarning::AccessoryTrustPending {
                entry: 0,
                band: FreqRange::new(86e6, 110e6),
            })
    );
    let hops = s.plan().hops.clone();
    for st in run(&mut s, hops.len()) {
        let Purpose::Sweep { hop } = st.purpose else {
            panic!("{st:?}")
        };
        let h = &hops[hop as usize];
        let in_band = h.covers.lo_hz >= 86e6 - 1.0 && h.covers.hi_hz <= 110e6 + 1.0;
        assert_eq!(st.accessory, in_band, "{h:?}");
        if in_band {
            assert_eq!((st.gain_entry, st.gains.lna_db), (Some(0), 16.0));
        }
    }
    let r = s.request_tx_slot(&TxSlotRequest {
        center_hz: 146e6,
        duration_ns: S,
    });
    assert!(matches!(r, Err(SchedulerError::TxGated)));
}

// ---- T-032 follow-ups ----

fn verify_plan(extra: Value) -> hk_model::ScanPlan {
    plan(
        "T-032 verify",
        1,
        vec![region(902.0, 928.0, 1.0, None)],
        ScanPolicy::SweepThenDwell,
        vec![],
        json!({ "scheduler": extra }),
    )
}

#[test]
fn a_clipped_a_block_is_never_the_retune_or_rate_change_base() {
    let mut s = hackrf(&verify_plan(
        json!({ "sweeps_per_cycle": 1, "rate_change": true }),
    ));
    s.offer_poi(Poi {
        verify: true,
        ..poi(7, 915.2e6, 200e3, 1.0)
    })
    .unwrap();
    let group = group_of(&run(&mut s, 20), 7);
    assert_eq!(group.len(), 9);
    let a_seq = |pair: u8| {
        group
            .iter()
            .find(|st| {
                st.purpose
                    == Purpose::GainStep {
                        poi: 7,
                        pair,
                        slot: GainSlot::A,
                    }
            })
            .unwrap()
            .seq
    };
    let evaluate = |clip: &dyn Fn(&Purpose) -> bool| {
        let mut v = Verification::new(7, 3);
        for st in &group {
            v.record(st, capture(st, clip(&st.purpose))).unwrap();
        }
        let mut eval = Evaluator::default();
        let report = v.evaluate(&mut eval);
        (report, eval)
    };

    // A0 clipped: pair 0 is skipped and A1 is the base of every retune and the rate change.
    let (report, eval) = evaluate(&|p| {
        *p == Purpose::GainStep {
            poi: 7,
            pair: 0,
            slot: GainSlot::A,
        }
    });
    assert_eq!(
        (report.gain_pairs_run, report.gain_pairs_skipped_clipped),
        (2, 1)
    );
    assert_eq!((report.retunes_run, report.rate_changes_run), (2, 1));
    assert_eq!(eval.bases, vec![a_seq(1); 3], "{report:?}");

    // Every A clipped (no baseline in a gain-step group): nothing has a base.
    let (report, eval) = evaluate(&|p| {
        matches!(
            p,
            Purpose::GainStep {
                slot: GainSlot::A,
                ..
            }
        )
    });
    assert_eq!(
        (
            report.retunes_run,
            report.retunes_without_base,
            report.rate_changes_run,
            report.rate_changes_without_base
        ),
        (0, 2, 0, 1)
    );
    assert!(eval.bases.is_empty());

    // A clipped moved capture is skipped, never compared.
    let (report, eval) = evaluate(&|p| {
        matches!(p, Purpose::Retune { delta_hz, .. } if *delta_hz > 0.0)
            || matches!(p, Purpose::RateChange { .. })
    });
    assert_eq!(
        (
            report.retunes_run,
            report.retunes_skipped_clipped,
            report.rate_changes_skipped_clipped
        ),
        (1, 1, 1)
    );
    assert_eq!(eval.retunes[0].0, -1e6);
    assert_eq!(eval.bases, vec![a_seq(0)]);
}

#[test]
fn captures_of_a_restarted_verification_group_never_mix_with_the_old_group() {
    let v1 = verify_plan(json!({ "sweeps_per_cycle": 1, "gain_step_pairs": 1 }));
    let mut s = hackrf(&v1);
    s.offer_poi(Poi {
        verify: true,
        ..poi(7, 915.2e6, 200e3, 1.0)
    })
    .unwrap();
    let old = run(&mut s, 4);
    assert_eq!(old[0].verification_group, None);
    assert_eq!(
        old[3].purpose,
        Purpose::Retune {
            poi: 7,
            delta_hz: 1e6
        }
    );
    let old_group = old[1].seq;
    assert!(
        old[1..]
            .iter()
            .all(|st| st.verification_group == Some(old_group))
    );
    let mut collector = Verification::new(7, 1);
    for st in &old[1..] {
        collector.record(st, capture(st, false)).unwrap();
    }

    // Plan update mid-group: v2 drops the retunes, and the group restarts under a new id.
    let mut v2 = v1.clone();
    v2.version = 2;
    v2.extra = json!({ "scheduler": { "sweeps_per_cycle": 1, "gain_step_pairs": 1, "retune_delta_hz": 0.0 } });
    s.update_plan(
        &v2,
        SchedulerConfig::from_plan(&v2).unwrap(),
        &mut MemorySurveyLog::default(),
        &SurveySummary::default(),
    )
    .unwrap();
    assert_eq!(
        s.poi_notes(7).unwrap().retunes,
        [RetunePlan::Skipped(RetuneSkip::Disabled); 2]
    );
    let new: Vec<ScheduleStep> = run(&mut s, 6)
        .into_iter()
        .filter(|st| st.verification_group.is_some())
        .collect();
    assert_eq!(new.len(), 2, "A0 and B0 only: {new:#?}");
    let new_group = new[0].verification_group.unwrap();
    assert!(new_group > old_group);
    for st in &new {
        collector.record(st, capture(st, false)).unwrap();
    }
    assert_eq!(collector.group(), Some(new_group));
    assert!(matches!(
        collector.record(&old[3], capture(&old[3], false)),
        Err(VerifyError::StaleGroup { current, .. }) if current == new_group
    ));
    let mut eval = Evaluator::default();
    let report = collector.evaluate(&mut eval);
    assert_eq!(
        (
            report.gain_pairs_run,
            report.retunes_run,
            report.retunes_without_base
        ),
        (1, 0, 0),
        "the old retune never meets the new A0"
    );

    // Same after a preemption cut: the restarted group has a new id.
    let mut s = hackrf(&v1);
    s.offer_poi(Poi {
        verify: true,
        ..poi(7, 915.2e6, 200e3, 1.0)
    })
    .unwrap();
    let first = run(&mut s, 3);
    s.preempt(UserIntent {
        id: 1,
        center_hz: 915e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: Some(S),
    })
    .unwrap();
    let again = run(&mut s, 3);
    assert_eq!(again[1].purpose, first[1].purpose);
    assert!(again[1].verification_group.unwrap() > first[1].verification_group.unwrap());
    let mut collector = Verification::new(7, 1);
    collector
        .record(&again[1], capture(&again[1], false))
        .unwrap();
    assert!(matches!(
        collector.record(&first[2], capture(&first[2], false)),
        Err(VerifyError::StaleGroup { .. })
    ));
    // A discovery step is not part of any group.
    assert!(matches!(
        collector.record(&first[0], capture(&first[0], false)),
        Err(VerifyError::NotThisVerification { .. })
    ));
}

#[test]
fn a_backwards_wall_clock_step_neither_reorders_steps_nor_rolls_back_a_finished_step() {
    let p = plan(
        "clock",
        1,
        vec![region(88.0, 148.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    let caps = SourceCapabilities::hackrf_one();
    let cfg = SchedulerConfig::from_plan(&p).unwrap();
    let back = |wall: &SyntheticClock, secs: i64| {
        wall.set(Timestamp::from_unix_nanos(T0_NS - secs * S));
    };

    // Monotonic clock anchored to wall time at start (what WallClock does with Instant).
    let wall = clock();
    let mono = SyntheticClock::new(Timestamp::from_unix_nanos(0));
    let mut s = Scheduler::new(
        &p,
        cfg.clone(),
        &caps,
        AnchoredClock::new(&wall, mono.clone()),
    )
    .unwrap();
    let a = s.next_step();
    assert_eq!(a.t_start.as_unix_nanos(), T0_NS);
    mono.advance_ns(a.duration_ns);
    back(&wall, 3600);
    let b = s.next_step();
    assert_eq!(b.t_start, a.t_end(), "ordering survives the wall step");
    mono.advance_ns(b.duration_ns);
    back(&wall, 7200);
    s.preempt(UserIntent {
        id: 1,
        center_hz: 100e6,
        rate_hz: 10e6,
        gains: None,
        duration_ns: Some(S),
    })
    .unwrap();
    assert_eq!(
        s.stats().truncated_slots,
        0,
        "the finished step is not rolled back"
    );
    let c = s.next_step();
    assert_eq!(c.purpose, Purpose::UserIntent { intent: 1 });
    assert_eq!(c.t_start, b.t_end());

    // Even a raw clock stepping backwards cannot reorder step start times.
    let wall = clock();
    let mut raw = Scheduler::new(&p, cfg, &caps, wall.clone()).unwrap();
    let first = raw.next_step();
    back(&wall, 3600);
    assert!(raw.next_step().t_start >= first.t_start);

    // The live clock is monotonic.
    use hk_core::scheduler::{Clock, WallClock};
    let (x, y) = (WallClock.now(), WallClock.now());
    assert!(y >= x);
}

#[test]
fn an_overflowing_score_or_a_zero_weight_does_not_starve_a_poi() {
    let p = plan(
        "weights",
        1,
        vec![region(902.0, 928.0, 2.0, None)],
        ScanPolicy::SweepThenDwell,
        vec![],
        Value::Null,
    );
    for (strong, weak) in [(f64::MAX, 1.0), (1.0, 0.0)] {
        let mut s = hackrf(&p);
        s.offer_poi(poi(1, 915e6, 100e3, strong)).unwrap();
        s.offer_poi(poi(2, 906e6, 100e3, weak)).unwrap();
        let steps = run(&mut s, 400);
        let n = |key| {
            steps
                .iter()
                .filter(|st| st.purpose == Purpose::Dwell { poi: key })
                .count()
        };
        let (n1, n2) = (n(1), n(2));
        assert_eq!(n1 + n2, 80);
        assert!(n2 >= 3 && n1 > 10 * n2, "{strong} vs {weak}: {n1}:{n2}");
    }
}

#[test]
fn retunes_keep_wide_pois_clear_of_dc_and_inside_the_span_or_are_skipped_with_a_reason() {
    let mut s = hackrf(&verify_plan(
        json!({ "sweeps_per_cycle": 1, "retune_delta_hz": 1.5e6, "gain_step_pairs": 1 }),
    ));
    let pois = [
        poi(1, 915e6, 400e3, 1.0),
        poi(2, 921e6, 3.5e6, 1.0),
        poi(3, 910e6, 8e6, 1.0),
        poi(4, 915e6, 18e6, 1.0),
    ];
    for p in pois {
        s.offer_poi(Poi { verify: true, ..p }).unwrap();
    }
    let notes = |k| s.poi_notes(k).unwrap();

    // 400 kHz at 8 Msps, 1.5 MHz off DC: +1.5 MHz would centre it on DC, −1.5 MHz would push it
    // past the 3 MHz half span; both shrink.
    let n1 = notes(1);
    assert!(!n1.on_dc && !n1.wider_than_span);
    match n1.retunes {
        [
            RetunePlan::Shrunk {
                delta_hz: up,
                configured_hz: cu,
            },
            RetunePlan::Shrunk {
                delta_hz: down,
                configured_hz: cd,
            },
        ] => {
            assert_eq!((cu, cd), (1.5e6, -1.5e6));
            assert!(
                (up - 1.29e6).abs() < 1.0 && (down + 1.3e6).abs() < 1.0,
                "{n1:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    // 3.5 MHz at 10 Msps: no offset large enough to separate products fits either way.
    assert_eq!(
        notes(2).retunes,
        [
            RetunePlan::Skipped(RetuneSkip::CrossesDc),
            RetunePlan::Skipped(RetuneSkip::PastUsableEdge)
        ]
    );
    // 8 MHz in a 15 MHz span straddles DC; 18 MHz exceeds the span: both noted, no retunes.
    for (key, wider) in [(3, false), (4, true)] {
        let n = notes(key);
        assert!(n.on_dc && n.wider_than_span == wider, "{key}: {n:?}");
        assert_eq!(n.retunes, [RetunePlan::Skipped(RetuneSkip::NoRoom); 2]);
    }

    let steps = run(&mut s, 80);
    for p in pois {
        let group = group_of(&steps, p.key);
        assert!(!group.is_empty(), "POI {} verified", p.key);
        let retunes: Vec<_> = group
            .iter()
            .filter(|st| matches!(st.purpose, Purpose::Retune { .. }))
            .collect();
        assert_eq!(
            retunes.len(),
            if p.key == 1 { 2 } else { 0 },
            "POI {}",
            p.key
        );
        for st in retunes {
            let usable_half = st.rate_hz * 0.75 / 2.0;
            let offset = (p.center_hz - st.center_hz).abs();
            assert!(offset - p.bandwidth_hz / 2.0 > 0.0, "clear of DC: {st:?}");
            assert!(
                offset + p.bandwidth_hz / 2.0 <= usable_half + 1.0,
                "inside the span: {st:?}"
            );
        }
    }
}

#[test]
fn frequent_plan_updates_still_reach_the_tail_of_the_pass() {
    let mut p = plan(
        "resume",
        1,
        vec![region(100.0, 190.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    let mut s = hackrf(&p);
    let hops = s.plan().hops.len();
    assert_eq!(hops, 6);
    let mut visited = vec![false; hops];
    let mut log = MemorySurveyLog::default();
    for _ in 0..hops {
        for st in run(&mut s, 2) {
            let Purpose::Sweep { hop } = st.purpose else {
                panic!("{st:?}")
            };
            visited[hop as usize] = true;
        }
        p.version += 1;
        s.update_plan(
            &p,
            SchedulerConfig::from_plan(&p).unwrap(),
            &mut log,
            &SurveySummary::default(),
        )
        .unwrap();
    }
    assert!(visited.iter().all(|&v| v), "{visited:?}");
    assert_eq!(s.passes_completed(), 2);
}

#[test]
fn a_seam_guard_keeps_hop_seams_inside_the_passband() {
    let region_plan = |guard: f64| {
        plan(
            "seams",
            1,
            vec![region(100.0, 190.0, 1.0, None)],
            ScanPolicy::SweepOnly,
            vec![],
            json!({ "scheduler": { "seam_guard_fraction": guard } }),
        )
    };
    let plain = hackrf(&region_plan(0.0)).plan().clone();
    let guarded = hackrf(&region_plan(0.2)).plan().clone();
    assert_eq!((plain.hops.len(), guarded.hops.len()), (6, 8));
    let usable = guarded.usable_span_hz;
    for h in &guarded.hops {
        let half = 0.8 * usable / 2.0 + 1.0;
        assert!(h.covers.hi_hz - h.center_hz <= half && h.center_hz - h.covers.lo_hz <= half);
    }
    let mut covers: Vec<FreqRange> = guarded.hops.iter().map(|h| h.covers).collect();
    covers.sort_by(|a, b| a.lo_hz.total_cmp(&b.lo_hz));
    for w in covers.windows(2) {
        assert!((w[0].hi_hz - w[1].lo_hz).abs() < 1.0, "gap-free tiling");
    }
    assert!(matches!(
        SchedulerConfig::from_plan(&region_plan(0.5))
            .unwrap()
            .validate(&SourceCapabilities::hackrf_one()),
        Err(PlanError::InvalidConfig(_))
    ));
}

#[test]
fn rf_path_boundaries_come_from_the_source_unless_overridden() {
    let p = plan(
        "paths",
        1,
        vec![region(2100.0, 2800.0, 1.0, None)],
        ScanPolicy::SweepOnly,
        vec![],
        Value::Null,
    );
    let paths = |caps: &SourceCapabilities, cfg: SchedulerConfig| {
        let s = Scheduler::new(&p, cfg, caps, clock()).unwrap();
        let mut v: Vec<u8> = s.plan().hops.iter().map(|h| h.rf_path).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let hackrf = SourceCapabilities::hackrf_one();
    assert_eq!(paths(&hackrf, SchedulerConfig::default()), vec![0, 1, 2]);
    let single = SourceCapabilities {
        rf_path_boundaries_hz: Vec::new(),
        ..hackrf.clone()
    };
    assert_eq!(paths(&single, SchedulerConfig::default()), vec![0]);
    let overridden = SchedulerConfig {
        rf_path_boundaries_hz: vec![2500e6],
        ..SchedulerConfig::default()
    };
    assert_eq!(paths(&hackrf, overridden), vec![0, 1]);
}
