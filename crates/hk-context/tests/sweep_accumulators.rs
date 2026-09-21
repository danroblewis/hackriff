//! T-558 measurement: what a device-wide survey costs the learned channel plan.
//!
//! A 1 MHz–6 GHz sweep meets a great many emitters. `ChannelPlan::learn` is the survey's
//! accumulator, so the question this file answers is not "does it work" but "what does it
//! grow to, and what does a step cost once it has".
//!
//! `measure_channel_plan_growth_over_a_full_sweep` (`--ignored --nocapture`) prints the table.
//! The rest assert the **bound**: what the plan holds, and what a step costs, must not be
//! functions of how far the survey has run.
//!
//! Measured on this machine before the T-558 fix: 236 of 2999 steps in 120 s, 1896 clusters and
//! still climbing, **1.46 s for one step** and rising quadratically. After: all 2999 steps in
//! 0.65 s, 4096 clusters, 0.2 ms a step.

use std::time::Instant;

use hk_context::occupancy::channels::{ChannelPlan, DetectionExtent, LearnConfig};
use hk_model::{FreqRange, TimeRange, Timestamp};

/// One sweep step's worth of detections, at `center_hz`, `n` of them spread over `span_hz`.
fn step(center_hz: f64, span_hz: f64, n: usize, t_ns: i64) -> Vec<DetectionExtent> {
    (0..n)
        .map(|i| {
            let f = center_hz - 0.5 * span_hz + span_hz * (i as f64 + 0.5) / n as f64;
            let obw = 12.5e3;
            let start = Timestamp::from_unix_nanos(t_ns + i as i64 * 1_000_000);
            DetectionExtent {
                time: TimeRange::new(start, start.saturating_add_nanos(5_000_000)),
                freq: FreqRange::centered(f, obw),
                obw_hz: obw,
                snr_db: 20.0,
                suspect: false,
            }
        })
        .collect()
}

#[test]
#[ignore = "measurement, not an assertion"]
fn measure_channel_plan_growth_over_a_full_sweep() {
    let mut plan = ChannelPlan::new(1, 6250.0, LearnConfig::default());
    let (lo, hi, span) = (1e6, 6_000e6, 2e6);
    let steps = ((hi - lo) / span) as usize;
    let per_step = 8;
    let t0 = Instant::now();
    let mut t_ns = 1_700_000_000_000_000_000i64;
    println!("step  centre_MHz  clusters  channels  step_ms  total_s");
    for i in 0..steps {
        let c = lo + span * (i as f64 + 0.5);
        let s = Instant::now();
        plan.learn(&step(c, 0.8 * span, per_step, t_ns));
        t_ns += 1_000_000_000;
        let dt = s.elapsed();
        if i % 250 == 0 || dt.as_millis() > 200 {
            println!(
                "{i:5}  {:10.1}  {:8}  {:8}  {:7.1}  {:7.1}",
                c / 1e6,
                plan.cluster_count(),
                plan.channels().len(),
                dt.as_secs_f64() * 1e3,
                t0.elapsed().as_secs_f64()
            );
        }
        if t0.elapsed().as_secs_f64() > 120.0 {
            println!("STOPPED after {i} of {steps} steps: 120 s budget spent");
            break;
        }
    }
    println!(
        "final clusters {} channels {} after {:.1} s",
        plan.cluster_count(),
        plan.channels().len(),
        t0.elapsed().as_secs_f64()
    );
}

/// Runs `steps` sweep steps and returns the clusters left.
fn sweep(steps: usize, per_step: usize, cfg: LearnConfig) -> ChannelPlan {
    let mut plan = ChannelPlan::new(1, 6250.0, cfg);
    let (lo, span) = (1e6, 2e6);
    let mut t_ns = 1_700_000_000_000_000_000i64;
    for i in 0..steps {
        let c = lo + span * (i as f64 + 0.5);
        plan.learn(&step(c, 0.8 * span, per_step, t_ns));
        t_ns += 1_000_000_000;
    }
    plan
}

/// The bound, stated as a bound: four times the survey must not mean four times the accumulator.
///
/// Before the fix `clusters` was `steps × per_step` with nothing ever removed, so this read
/// `4000 <= 4096` and then `16000 <= 4096` — red on the second, which is the point of running it
/// at two lengths rather than one.
#[test]
fn the_channel_plan_holds_a_bounded_number_of_clusters_however_long_the_survey_runs() {
    let cfg = LearnConfig::default();
    let short = sweep(1000, 8, cfg);
    let long = sweep(4000, 8, cfg);
    for (steps, plan) in [(1000, &short), (4000, &long)] {
        assert!(
            plan.cluster_count() <= cfg.max_clusters,
            "{steps} steps: {} clusters exceeds the {} cap",
            plan.cluster_count(),
            cfg.max_clusters
        );
    }
    assert_eq!(
        short.cluster_count(),
        long.cluster_count(),
        "four times the survey is the same residency: the cap is a cap, not a slower climb"
    );
}

/// A region that goes quiet stops costing anything: an unpublished cluster unseen for longer than
/// the whole persistence window is forgotten, while a published channel is kept.
///
/// The same sequence is run with expiry switched off, so the assertion is about the expiry and
/// not about some other reason the count might come out low.
#[test]
fn an_unpublished_cluster_expires_once_its_region_has_been_quiet() {
    fn run(cfg: LearnConfig) -> (usize, usize) {
        let mut plan = ChannelPlan::new(1, 6250.0, cfg);
        let t0 = 1_700_000_000_000_000_000i64;
        // Two detections at one place: enough evidence, strong enough to publish.
        plan.learn(&step(100e6, 1.0, 1, t0));
        plan.learn(&step(100e6, 1.0, 1, t0 + 1_000_000_000));
        // And one lone detection elsewhere, which will never recur.
        plan.learn(&step(200e6, 1.0, 1, t0 + 2_000_000_000));
        assert_eq!(plan.channels().len(), 1, "the recurring place published");
        assert_eq!(
            plan.cluster_count(),
            2,
            "both places held while both are recent"
        );
        // Time passes, in a part of the spectrum neither of them is in.
        let quiet = t0 + LearnConfig::default().forget_unpublished_ns + 10_000_000_000;
        plan.learn(&step(300e6, 1.0, 1, quiet));
        (plan.cluster_count(), plan.channels().len())
    }

    let (kept, channels) = run(LearnConfig {
        forget_unpublished_ns: 0,
        ..LearnConfig::default()
    });
    assert_eq!(
        (kept, channels),
        (3, 1),
        "with expiry off the one-off at 200 MHz is held for ever"
    );

    let (kept, channels) = run(LearnConfig::default());
    assert_eq!(
        channels, 1,
        "the published channel survives the quiet: it is measured structure, not a hypothesis"
    );
    assert_eq!(
        kept, 2,
        "the unpublished one-off at 200 MHz should have been forgotten after the quiet, \
         leaving the published channel and the new detection"
    );
}

/// A whole 1 MHz-6 GHz survey's worth of learning finishes in seconds, not hours.
///
/// The budget is deliberately loose (a debug build on a loaded machine): before the fix this same
/// loop needed **over 120 seconds to reach step 236 of 2999**, because `merge` compared every
/// pair of clusters after every detection and recomputed four medians per comparison. Any return
/// to quadratic-in-survey-length cost blows a 60 s budget long before it blows a tight one.
#[test]
fn a_full_sweep_of_learning_costs_a_bounded_time_per_step() {
    let t0 = Instant::now();
    let plan = sweep(2999, 8, LearnConfig::default());
    let dt = t0.elapsed();
    assert!(
        dt.as_secs_f64() < 60.0,
        "2999 sweep steps took {:.1} s ({} clusters): the per-step cost is growing with the survey",
        dt.as_secs_f64(),
        plan.cluster_count()
    );
}
