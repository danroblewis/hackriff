//! T-114 simulator tests: determinism, POI against the docs/04 §3.8 formula, pure-sweep burst
//! misses, round-robin revisit, registry, and a release-build speed check.

use std::time::Instant;

use hk_core::SourceCapabilities;
use hk_sim::report::PolicyReport;
use hk_sim::scenario::{MHZ, S, band};
use hk_sim::{
    EmitterClass, Policy, PolicyRegistry, RadioModel, RunConfig, Scenario, ScenarioConfig,
    SchedulerPolicy, SimEnv, compare, run_policy,
};

/// A population with nothing in it; tests add what they need.
fn empty_config(regions_mhz: &[(f64, f64)]) -> ScenarioConfig {
    ScenarioConfig {
        name: "test".into(),
        regions_hz: regions_mhz
            .iter()
            .map(|&(lo, hi)| (lo * MHZ, hi * MHZ))
            .collect(),
        carriers: 0,
        bursts: 0,
        beacons: 0,
        hoppers: Vec::new(),
        imd_ghosts: 0,
        injected: Vec::new(),
        fade_db: 0.0,
        diurnal_depth: 0.0,
        ..ScenarioConfig::default()
    }
}

fn run_named(scenario: &Scenario, radio: &RadioModel, policy: &mut dyn Policy) -> PolicyReport {
    run_policy(scenario, radio, &RunConfig::default(), policy)
}

fn env<'a>(
    scenario: &'a Scenario,
    radio: &'a RadioModel,
    caps: &'a SourceCapabilities,
) -> SimEnv<'a> {
    SimEnv {
        scenario,
        radio,
        caps,
    }
}

#[test]
fn same_seed_same_report_and_seeds_differ() {
    let radio = RadioModel::default();
    let cfg = RunConfig::default();
    let registry = PolicyRegistry::baselines();
    let report = |seed| {
        let scenario = Scenario::generate(&ScenarioConfig::default(), seed, 240 * S);
        let r = compare(&scenario, &radio, &cfg, &registry, &[]).unwrap();
        serde_json::to_string(&r).unwrap()
    };
    let a = report(11);
    assert_eq!(a, report(11), "same seed must give the same report");
    assert_ne!(a, report(12));
    let v: serde_json::Value = serde_json::from_str(&a).unwrap();
    let names: Vec<&str> = v["policies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["pure-sweep", "round-robin-dwell", "wrr"]);
    for p in v["policies"].as_array().unwrap() {
        assert_eq!(p["ttfd"]["injected"].as_array().unwrap().len(), 2);
        assert_eq!(p["regions"].as_array().unwrap().len(), 4);
        assert!(
            p["discovery"]["discovered"].as_u64().unwrap() > 0,
            "{}",
            p["name"]
        );
    }
    let wrr = &v["policies"][2];
    assert!(
        wrr["time"]["stream_s"].as_f64().unwrap() > 0.0,
        "wrr dwells on POIs"
    );
    assert!(wrr["time"]["mode_switches"].as_u64().unwrap() > 0);
}

/// Short bursts (τ 5–100 ms, Poisson) in three regions; the per-region measured POI must match
/// min(1, (τ + T_d)/T_R) within statistical error for a periodic sweep and a periodic
/// round-robin dwell.
#[test]
fn simulated_poi_matches_formula_per_region() {
    let regions = [(400.0, 500.0), (800.0, 1000.0), (2400.0, 2500.0)];
    let mut cfg = empty_config(&regions);
    cfg.bursts = 450;
    cfg.burst_bands = regions.iter().map(|&(lo, hi)| band(lo, hi, 50.0)).collect();
    cfg.burst_tau_ms = (5.0, 100.0);
    cfg.burst_mean_gap_s = (0.5, 2.0);
    cfg.burst_snr_db = (30.0, 30.0);
    let scenario = Scenario::generate(&cfg, 5, 300 * S);
    let radio = RadioModel::default();
    let caps = SourceCapabilities::hackrf_one();
    let env = env(&scenario, &radio, &caps);

    let mut sweep = SchedulerPolicy::pure_sweep(&env).unwrap();
    let mut rr = SchedulerPolicy::round_robin_dwell(&env, S).unwrap();
    for (policy, slack) in [(&mut sweep, 4.0), (&mut rr, 6.0)] {
        let r = run_named(&scenario, &radio, policy);
        for region in &r.regions {
            let poi = &region.poi;
            let (m, p, se) = (
                poi.measured.unwrap(),
                poi.predicted.unwrap(),
                poi.std_err.unwrap(),
            );
            assert!(poi.transmissions > 5_000, "{}: {poi:?}", r.name);
            assert!(
                (m - p).abs() <= slack * se + 0.002,
                "{} region {}: measured {m:.4} predicted {p:.4} se {se:.4}",
                r.name,
                region.region
            );
        }
    }
}

/// Pure sweep over 1 MHz–6 GHz: ~0.75 s revisit, sub-ms live dwell, so a 5 ms burst is caught
/// with P ≈ (τ + T_d)/T_R ≈ 0.7 % (docs/04 §3.8 worked example) and missed the rest of the time.
#[test]
fn pure_sweep_misses_short_bursts_at_expected_rate() {
    let mut cfg = empty_config(&[(1.0, 6000.0)]);
    cfg.bursts = 400;
    cfg.burst_bands = vec![band(2.0, 5990.0, 50.0)];
    cfg.burst_tau_ms = (5.0, 5.0);
    cfg.burst_mean_gap_s = (0.5, 1.0);
    cfg.burst_snr_db = (30.0, 30.0);
    let scenario = Scenario::generate(&cfg, 9, 90 * S);
    let radio = RadioModel::default();
    let caps = SourceCapabilities::hackrf_one();
    let mut sweep = SchedulerPolicy::pure_sweep(&env(&scenario, &radio, &caps)).unwrap();
    let pass_ns = sweep.scheduler().plan().pass_ns as f64;
    assert!(
        (0.7..0.8).contains(&(pass_ns / S as f64)),
        "full sweep {pass_ns} ns"
    );
    let r = run_named(&scenario, &radio, &mut sweep);

    let burst = r
        .classes
        .iter()
        .find(|c| c.class == EmitterClass::Burst)
        .unwrap();
    let n = burst.transmissions as f64;
    let t_d = (radio.sweep_step_ns - radio.sweep_settle_ns) as f64;
    let expected = (5e6 + t_d) / pass_ns;
    assert!((0.006..0.008).contains(&expected), "{expected}");
    let measured = burst.captured as f64 / n;
    let se = (expected * (1.0 - expected) / n).sqrt();
    assert!(n > 30_000.0, "{n}");
    assert!(
        (measured - expected).abs() <= 4.0 * se,
        "measured {measured:.5} expected {expected:.5} se {se:.5}"
    );
    let miss = 1.0 - measured;
    assert!(
        miss > 0.99,
        "pure sweep misses > 99 % of 5 ms bursts: {miss}"
    );
}

/// Round-robin dwell visits every window once per pass: each region's T_R is the pass length.
#[test]
fn round_robin_revisit_matches_its_schedule() {
    let cfg = empty_config(&[
        (1.0, 300.0),
        (300.0, 1000.0),
        (1000.0, 3000.0),
        (3000.0, 6000.0),
    ]);
    let dwell_ns = S / 4;
    let radio = RadioModel::default();
    let caps = SourceCapabilities::hackrf_one();
    let probe = Scenario::generate(&cfg, 1, S);
    let pass_ns = SchedulerPolicy::round_robin_dwell(&env(&probe, &radio, &caps), dwell_ns)
        .unwrap()
        .scheduler()
        .plan()
        .pass_ns;
    let windows = pass_ns / dwell_ns;
    assert!((395..=410).contains(&windows), "{windows} windows");
    let scenario = Scenario::generate(&cfg, 1, 3 * pass_ns + pass_ns / 2);
    let mut rr =
        SchedulerPolicy::round_robin_dwell(&env(&scenario, &radio, &caps), dwell_ns).unwrap();
    let r = run_named(&scenario, &radio, &mut rr);
    let pass_s = pass_ns as f64 / S as f64;
    // Intervals run between live-window starts; only the very first window pays the mode-switch
    // dead time instead of a stream retune, so a first-pass interval can differ by that much.
    let first_window_s = (radio.mode_switch_ns - radio.stream_retune_ns) as f64 / S as f64;
    for region in &r.regions {
        assert_eq!(region.bins_unvisited, 0, "{region:?}");
        let mean = region.revisit_mean_s.unwrap();
        let max = region.revisit_max_s.unwrap();
        assert!(
            (mean - pass_s).abs() <= first_window_s + 1e-9,
            "region {}: mean {mean} vs pass {pass_s}",
            region.region
        );
        // T-173: sweep plans dither hop centres on alternate passes (dc_dither_hz) so every cell gets
        // an off-DC view. A cell near a hop boundary can then be served by hop k on even passes and
        // hop k±1 on odd passes, so one interval may stretch by exactly one dwell. The mean stays one
        // pass (asserted above); the max is still bounded, by one pass plus one dwell.
        let dwell_s = dwell_ns as f64 / S as f64;
        assert!(
            max - pass_s <= first_window_s + dwell_s + 1e-9
                && pass_s - max <= first_window_s + 1e-9,
            "region {}: max {max} vs pass {pass_s} (+ one dwell {dwell_s})",
            region.region
        );
    }
}

/// A policy registered by name runs like a baseline (the T-120 bandit hook).
#[test]
fn registry_builds_registered_policies() {
    let mut registry = PolicyRegistry::baselines();
    registry.register(
        "sweep-alias",
        Box::new(|env| Ok(Box::new(SchedulerPolicy::pure_sweep(env)?) as Box<dyn Policy>)),
    );
    assert_eq!(
        registry.names(),
        ["pure-sweep", "round-robin-dwell", "wrr", "sweep-alias"]
    );
    let scenario = Scenario::generate(&ScenarioConfig::default(), 3, 30 * S);
    let radio = RadioModel::default();
    let caps = SourceCapabilities::hackrf_one();
    assert!(
        registry
            .build("nope", &env(&scenario, &radio, &caps))
            .is_err()
    );
    let r = compare(
        &scenario,
        &radio,
        &RunConfig::default(),
        &registry,
        &["sweep-alias"],
    )
    .unwrap();
    assert_eq!(r.policies.len(), 1);
    assert_eq!(r.policies[0].name, "pure-sweep");
}

/// One simulated day of the default scenario under each baseline, in a release build.
#[test]
#[cfg_attr(debug_assertions, ignore = "speed check: run with --release")]
fn one_simulated_day_runs_in_seconds() {
    let scenario = Scenario::generate(&ScenarioConfig::default(), 1, 86_400 * S);
    let radio = RadioModel::default();
    let registry = PolicyRegistry::baselines();
    let started = Instant::now();
    let r = compare(&scenario, &radio, &RunConfig::default(), &registry, &[]).unwrap();
    let elapsed = started.elapsed().as_secs_f64();
    assert_eq!(r.policies.len(), 3);
    assert!(
        elapsed < 60.0,
        "1 simulated day x 3 policies took {elapsed:.1} s"
    );
}
