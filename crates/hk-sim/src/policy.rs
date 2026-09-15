//! The [`Policy`] trait the simulator drives, the registry policies are built from, and the
//! baseline policies over the existing hk-core [`Scheduler`].
//!
//! A policy decides where the single radio points next ([`Policy::next_step`], returning an
//! hk-core [`ScheduleStep`]) and receives what the radio saw ([`Policy::observe`]). Policies never
//! see the truth population: detections carry measured centre, bandwidth, SNR and the C05
//! suspect flag, keyed by a tracker id.
//!
//! Baselines (all driven through `hk_core::scheduler::Scheduler` on a shared synthetic clock):
//! - `pure-sweep`: a `SweepOnly` plan with sweep hops matched to the radio's firmware sweep steps.
//! - `round-robin-dwell`: a `DwellOnly` plan: stream windows tiling the regions, one fixed dwell
//!   each, in order.
//! - `wrr`: the current v1 scheduler (`SweepThenDwell`): one full sweep pass, then
//!   `dwells_per_pass` POI dwell slots shared by weighted round robin; every detection is offered
//!   as a POI (the current pipeline behaviour, C05 suspect flags not honoured).

use std::collections::BTreeSet;

use hk_core::scheduler::{CompiledPlan, Poi, Scheduler, SchedulerError, SyntheticClock};
use hk_core::{ScheduleStep, SourceCapabilities};
use hk_model::{ScanPolicy, Timestamp};
use serde_json::{Value, json};

use crate::SimError;
use crate::radio::{RadioMode, RadioModel};
use crate::scenario::{S, Scenario, T0_NS};

/// One detected emitter in one observation window (aggregated over its transmissions there).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Detection {
    /// Tracker key (stable per emitter; the POI key policies should use).
    pub key: u64,
    /// Measured centre of the last detected transmission, Hz.
    pub center_hz: f64,
    /// Measured bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Highest measured SNR, dB.
    pub snr_db: f64,
    /// Tagged suspect by the C05 trust tests (IMD ghost, spur).
    pub suspect: bool,
    /// Transmissions detected in the window.
    pub transmissions: u32,
    /// Still on at the end of the window and on for the whole window.
    pub continuous: bool,
}

/// What the radio saw during one step.
#[derive(Clone, Copy, Debug)]
pub struct Observation<'a> {
    /// Mode the step ran in.
    pub mode: RadioMode,
    /// Live window start (after retune / mode-switch dead time). `start >= end`: nothing observed.
    pub start: Timestamp,
    /// Live window end.
    pub end: Timestamp,
    /// Observed band `(lo, hi)`, Hz.
    pub band_hz: (f64, f64),
    /// Detections.
    pub detections: &'a [Detection],
}

/// What a policy factory gets.
#[derive(Clone, Copy, Debug)]
pub struct SimEnv<'a> {
    /// Scenario (policies may use its regions and duration, never its emitters).
    pub scenario: &'a Scenario,
    /// Radio model.
    pub radio: &'a RadioModel,
    /// Source capabilities.
    pub caps: &'a SourceCapabilities,
}

/// An attention policy under simulation.
pub trait Policy {
    /// Stable name used in reports.
    fn name(&self) -> &str;

    /// Parameters for the report.
    fn params(&self) -> Value {
        Value::Null
    }

    /// The step starting at `now`. `duration_ns` must be > 0.
    fn next_step(&mut self, now: Timestamp) -> ScheduleStep;

    /// What the radio saw during `step` (called once per step, in order).
    fn observe(&mut self, _step: &ScheduleStep, _obs: &Observation<'_>) {}
}

/// Builds a policy for an environment.
pub type PolicyFactory = Box<dyn Fn(&SimEnv<'_>) -> Result<Box<dyn Policy>, SimError>>;

/// Named policy factories. T-120 registers its bandit with [`PolicyRegistry::register`].
pub struct PolicyRegistry {
    entries: Vec<(String, PolicyFactory)>,
}

impl Default for PolicyRegistry {
    fn default() -> Self {
        Self::baselines()
    }
}

impl PolicyRegistry {
    /// An empty registry.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// `pure-sweep`, `round-robin-dwell` and `wrr` with their default parameters.
    pub fn baselines() -> Self {
        let mut r = Self::empty();
        r.register(
            PURE_SWEEP,
            Box::new(|env| Ok(Box::new(SchedulerPolicy::pure_sweep(env)?) as Box<dyn Policy>)),
        );
        r.register(
            ROUND_ROBIN_DWELL,
            Box::new(|env| {
                Ok(Box::new(SchedulerPolicy::round_robin_dwell(env, S)?) as Box<dyn Policy>)
            }),
        );
        r.register(
            WRR,
            Box::new(|env| {
                Ok(Box::new(SchedulerPolicy::wrr(env, WrrParams::default())?) as Box<dyn Policy>)
            }),
        );
        r
    }

    /// Adds or replaces a factory.
    pub fn register(&mut self, name: &str, factory: PolicyFactory) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.0 == name) {
            e.1 = factory;
        } else {
            self.entries.push((name.to_owned(), factory));
        }
    }

    /// Registered names in registration order.
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.0.as_str()).collect()
    }

    /// Builds the policy `name`.
    pub fn build(&self, name: &str, env: &SimEnv<'_>) -> Result<Box<dyn Policy>, SimError> {
        let (_, f) = self
            .entries
            .iter()
            .find(|e| e.0 == name)
            .ok_or_else(|| SimError::UnknownPolicy(name.to_owned()))?;
        f(env)
    }
}

/// Name of the pure-sweep baseline.
pub const PURE_SWEEP: &str = "pure-sweep";
/// Name of the round-robin dwell baseline.
pub const ROUND_ROBIN_DWELL: &str = "round-robin-dwell";
/// Name of the current weighted-round-robin scheduler.
pub const WRR: &str = "wrr";

/// Parameters of the `wrr` policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WrrParams {
    /// POI dwell slots after each full sweep pass (2).
    pub dwells_per_pass: u32,
    /// POI dwell without a burst interval, ns (1 s).
    pub dwell_default_ns: i64,
    /// Interestingness offered with every POI (1: the v1 placeholder).
    pub interestingness: f64,
}

impl Default for WrrParams {
    fn default() -> Self {
        Self {
            dwells_per_pass: 2,
            dwell_default_ns: S,
            interestingness: 1.0,
        }
    }
}

/// A policy that is the hk-core scheduler on a synthetic clock set to simulation time.
pub struct SchedulerPolicy {
    name: String,
    params: Value,
    clock: SyntheticClock,
    sched: Scheduler<SyntheticClock>,
    offer_pois: Option<f64>,
    offered: BTreeSet<u64>,
}

impl SchedulerPolicy {
    /// Any plan and settings; `offer_pois` offers every detection as a POI with that
    /// interestingness.
    pub fn new(
        name: &str,
        params: Value,
        scheduler: Scheduler<SyntheticClock>,
        clock: SyntheticClock,
        offer_pois: Option<f64>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            params,
            clock,
            sched: scheduler,
            offer_pois,
            offered: BTreeSet::new(),
        }
    }

    /// Pure sweep: `SweepOnly`, hops matched to the radio's sweep steps.
    pub fn pure_sweep(env: &SimEnv<'_>) -> Result<Self, SimError> {
        let plan = env.scenario.plan(ScanPolicy::SweepOnly, Value::Null);
        let cfg = env.radio.sweep_scheduler_config();
        let clock = clock();
        let sched = Scheduler::new(&plan, cfg, env.caps, clock.clone())?;
        let params = json!({
            "plan_policy": "sweep-only",
            "hops_per_pass": sched.plan().hops.len(),
            "pass_s": sched.plan().pass_ns as f64 / S as f64,
        });
        Ok(Self::new(PURE_SWEEP, params, sched, clock, None))
    }

    /// Round-robin dwell: `DwellOnly`, stream windows of `dwell_ns` tiling the regions.
    pub fn round_robin_dwell(env: &SimEnv<'_>, dwell_ns: i64) -> Result<Self, SimError> {
        let plan = env.scenario.plan(ScanPolicy::DwellOnly, Value::Null);
        let mut cfg = env.radio.stream_scheduler_config();
        cfg.region_dwell_ns = dwell_ns;
        let clock = clock();
        let sched = Scheduler::new(&plan, cfg, env.caps, clock.clone())?;
        let params = json!({
            "plan_policy": "dwell-only",
            "dwell_s": dwell_ns as f64 / S as f64,
            "windows_per_pass": sched.plan().hops.len(),
            "pass_s": sched.plan().pass_ns as f64 / S as f64,
        });
        Ok(Self::new(ROUND_ROBIN_DWELL, params, sched, clock, None))
    }

    /// The current v1 WRR scheduler: `SweepThenDwell`, one pass then `dwells_per_pass` POI
    /// dwells; every detection offered as a POI.
    pub fn wrr(env: &SimEnv<'_>, p: WrrParams) -> Result<Self, SimError> {
        let plan = env.scenario.plan(ScanPolicy::SweepThenDwell, Value::Null);
        let mut cfg = env.radio.sweep_scheduler_config();
        cfg.validate(env.caps)?;
        let hops = CompiledPlan::compile(&plan, &cfg, env.caps)?.hops.len();
        cfg.sweeps_per_cycle = u32::try_from(hops).unwrap_or(u32::MAX).max(1);
        cfg.dwells_per_cycle = p.dwells_per_pass;
        cfg.dwell_default_ns = p.dwell_default_ns;
        let clock = clock();
        let sched = Scheduler::new(&plan, cfg, env.caps, clock.clone())?;
        let params = json!({
            "plan_policy": "sweep-then-dwell",
            "hops_per_pass": hops,
            "dwells_per_pass": p.dwells_per_pass,
            "dwell_default_s": p.dwell_default_ns as f64 / S as f64,
            "interestingness": p.interestingness,
            "max_pois": sched.config().max_pois,
            "honours_suspect": false,
        });
        Ok(Self::new(
            WRR,
            params,
            sched,
            clock,
            Some(p.interestingness),
        ))
    }

    /// The wrapped scheduler.
    pub fn scheduler(&self) -> &Scheduler<SyntheticClock> {
        &self.sched
    }
}

fn clock() -> SyntheticClock {
    SyntheticClock::new(Timestamp::from_unix_nanos(T0_NS))
}

impl Policy for SchedulerPolicy {
    fn name(&self) -> &str {
        &self.name
    }

    fn params(&self) -> Value {
        self.params.clone()
    }

    fn next_step(&mut self, now: Timestamp) -> ScheduleStep {
        self.clock.set(now);
        self.sched.next_step()
    }

    fn observe(&mut self, _step: &ScheduleStep, obs: &Observation<'_>) {
        let Some(interestingness) = self.offer_pois else {
            return;
        };
        for d in obs.detections {
            if self.offered.contains(&d.key) {
                continue;
            }
            let poi = Poi {
                key: d.key,
                center_hz: d.center_hz,
                bandwidth_hz: d.bandwidth_hz,
                interestingness,
                burst_interval_ns: None,
                verify: false,
            };
            match self.sched.offer_poi(poi) {
                Ok(()) => {
                    self.offered.insert(d.key);
                }
                // A full queue keeps its POIs (equal scores never evict): retry on a later sighting.
                Err(SchedulerError::QueueFull) => {}
                Err(_) => {
                    self.offered.insert(d.key);
                }
            }
        }
    }
}
