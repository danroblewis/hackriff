//! T-124 (AWARE-027): the scheduler simulator comparison of the bandit against round-robin dwell,
//! recorded as the hk-sim comparison report JSON (T-114/T-120) under the test target directory.
//!
//! The simulator's scenario is its own seeded synthetic population (hk-sim `Scenario`), so this is
//! a record, not a gate on policy quality: asserted are only that both policies ran on the same
//! scenario and discovered emitters. The numbers are printed and written to
//! `$CARGO_TARGET_TMPDIR/t124-sim-bandit-vs-round-robin.json`.

use std::time::Instant;

use hk_sim::bandit::{BANDIT, register};
use hk_sim::scenario::S;
use hk_sim::{
    EmitterClass, PolicyRegistry, ROUND_ROBIN_DWELL, RadioModel, RunConfig, Scenario,
    ScenarioConfig, compare,
};

const T124: &str = "T-124/sim";
/// Simulated hours (debug build: kept short).
const SIM_HOURS: i64 = 6;
const SEED: u64 = 1;

#[test]
fn m2_simulator_bandit_vs_round_robin_recorded() {
    let wall = Instant::now();
    let scenario = Scenario::generate(&ScenarioConfig::default(), SEED, SIM_HOURS * 3600 * S);
    let radio = RadioModel::default();
    let mut registry = PolicyRegistry::baselines();
    register(&mut registry);
    let report = compare(
        &scenario,
        &radio,
        &RunConfig::default(),
        &registry,
        &[ROUND_ROBIN_DWELL, BANDIT],
    )
    .unwrap();
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("t124-sim-bandit-vs-round-robin.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    eprintln!(
        "[{T124}] seed {SEED}, {SIM_HOURS} simulated h, wall {:.1} s, report {}",
        wall.elapsed().as_secs_f64(),
        path.display()
    );
    assert_eq!(report.policies.len(), 2);
    for p in &report.policies {
        let burst = p
            .classes
            .iter()
            .find(|c| c.class == EmitterClass::Burst)
            .expect("every class is reported");
        let poi: Vec<_> = p
            .regions
            .iter()
            .filter_map(|r| Some((r.poi.measured?, r.poi.predicted?)))
            .collect();
        let n = poi.len().max(1) as f64;
        eprintln!(
            "[{T124}] {:>18}: steps {}, discovered {}/{}, bursts captured/h {:.0} ({:.1} %), ttfd \
             median {:?} s p90 {:?} s, injected {:?}, suspect dwell {:.0} s (suspect-only {:.0} s), \
             mean POI measured {:.3} predicted {:.3}",
            p.name,
            p.time.steps,
            p.discovery.discovered,
            p.discovery.emitters,
            burst.captured_per_hour,
            100.0 * burst.capture_fraction.unwrap_or(0.0),
            p.ttfd.median_s.map(|t| t.round()),
            p.ttfd.p90_s.map(|t| t.round()),
            p.ttfd
                .injected
                .iter()
                .map(|i| (i.class.name(), i.ttfd_s.map(|t| t.round())))
                .collect::<Vec<_>>(),
            p.suspect.dwell_s_on_suspect_pois,
            p.suspect.dwell_s_suspect_only,
            poi.iter().map(|x| x.0).sum::<f64>() / n,
            poi.iter().map(|x| x.1).sum::<f64>() / n,
        );
        assert!(
            p.time.steps > 0 && p.discovery.discovered > 0,
            "[{T124}] {} ran and discovered emitters",
            p.name
        );
    }
    assert_eq!(report.policies[0].name, ROUND_ROBIN_DWELL);
    assert_eq!(report.policies[1].name, BANDIT);
}
