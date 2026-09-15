//! `hk-sim`: runs the attention-scheduler simulator and writes a JSON comparison report.
//!
//! `hk-sim --seed 1 --hours 24 --out report.json [--policy wrr ...]`

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use hk_core::SourceCapabilities;
use hk_sim::report::SCHEMA;
use hk_sim::scenario::S;
use hk_sim::{
    ComparisonReport, EmitterClass, PolicyRegistry, RadioModel, RunConfig, Scenario,
    ScenarioConfig, SimEnv, run_policy, summarize,
};

#[derive(Parser)]
#[command(about = "Discrete-event attention-scheduler simulator (C04, T-114)")]
struct Args {
    /// Scenario seed.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Simulated hours.
    #[arg(long, default_value_t = 24.0)]
    hours: f64,
    /// Policies to run (default: all registered).
    #[arg(long = "policy")]
    policies: Vec<String>,
    /// Report path (default: stdout).
    #[arg(long)]
    out: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let duration_ns = (args.hours * 3600.0 * S as f64) as i64;
    let scenario = Scenario::generate(&ScenarioConfig::default(), args.seed, duration_ns);
    let radio = RadioModel::default();
    let cfg = RunConfig::default();
    let caps = SourceCapabilities::hackrf_one();
    let env = SimEnv {
        scenario: &scenario,
        radio: &radio,
        caps: &caps,
    };
    let registry = PolicyRegistry::baselines();
    let names: Vec<String> = if args.policies.is_empty() {
        registry.names().into_iter().map(str::to_owned).collect()
    } else {
        args.policies
    };
    let mut policies = Vec::new();
    for name in &names {
        let mut policy = registry.build(name, &env)?;
        let started = Instant::now();
        let r = run_policy(&scenario, &radio, &cfg, policy.as_mut());
        let injected: Vec<String> = r
            .ttfd
            .injected
            .iter()
            .map(|i| match i.ttfd_s {
                Some(t) => format!("{}:{t:.1}s", i.class.name()),
                None => format!("{}:never", i.class.name()),
            })
            .collect();
        let burst = r
            .classes
            .iter()
            .find(|c| c.class == EmitterClass::Burst)
            .expect("every class is reported");
        eprintln!(
            "{name:>18}: {:.2}s wall, {} steps, discovered {}/{}, bursts captured/h {:.0} ({:.1}%), \
             ttfd median {:?}s, injected [{}], suspect dwell {:.0}s",
            started.elapsed().as_secs_f64(),
            r.time.steps,
            r.discovery.discovered,
            r.discovery.emitters,
            burst.captured_per_hour,
            100.0 * burst.capture_fraction.unwrap_or(0.0),
            r.ttfd.median_s.map(|v| (v * 10.0).round() / 10.0),
            injected.join(", "),
            r.suspect.dwell_s_on_suspect_pois,
        );
        policies.push(r);
    }
    let report = ComparisonReport {
        schema: SCHEMA,
        scenario: summarize(&scenario),
        radio,
        policies,
    };
    let json = serde_json::to_string_pretty(&report)?;
    match args.out {
        Some(path) => std::fs::write(path, json)?,
        None => println!("{json}"),
    }
    Ok(())
}
