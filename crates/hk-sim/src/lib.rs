//! Offline discrete-event simulator for the C04 attention scheduler (T-114).
//!
//! One half-duplex radio ([`RadioModel`]) is pointed by a [`Policy`] at a seeded truth population
//! ([`Scenario`]: continuous carriers, Poisson bursts, periodic beacons, hoppers, suspect IMD
//! ghosts, optional time-of-day modulation, emitters injected mid-run). Policies return hk-core
//! [`hk_core::ScheduleStep`]s, so the baselines are the existing
//! [`hk_core::scheduler::Scheduler`] under different plans ([`PolicyRegistry::baselines`]):
//! `pure-sweep`, `round-robin-dwell` and the current `wrr`. T-120 registers its bandit through
//! [`PolicyRegistry::register`].
//!
//! [`run_policy`] / [`compare`] produce a deterministic JSON-serialisable
//! [`ComparisonReport`]: emitters discovered vs time, transmissions captured per class per hour,
//! time to first detection (injected emitters and overall), revisit T_R per region, dwell spent
//! on suspect emitters, and per-region short-burst POI against
//! P_POI ≈ min(1, (τ + T_d)/T_R) (docs/04 §3.8).
//!
//! The `hk-sim` binary writes the comparison report.

pub mod emitter;
pub mod policy;
pub mod radio;
pub mod report;
pub mod rng;
pub mod scenario;
pub mod sim;

pub use policy::{
    Detection, Observation, PURE_SWEEP, Policy, PolicyFactory, PolicyRegistry, ROUND_ROBIN_DWELL,
    SchedulerPolicy, SimEnv, WRR, WrrParams,
};
pub use radio::{RadioMode, RadioModel};
pub use report::{ComparisonReport, PolicyReport};
pub use scenario::{EmitterClass, Scenario, ScenarioConfig};
pub use sim::{RunConfig, compare, run_policy, summarize};

/// Simulator errors.
#[derive(Debug, thiserror::Error)]
pub enum SimError {
    /// The scheduler rejected the plan or settings.
    #[error(transparent)]
    Scheduler(#[from] hk_core::scheduler::SchedulerError),
    /// The plan does not compile.
    #[error(transparent)]
    Plan(#[from] hk_core::scheduler::PlanError),
    /// No policy of that name is registered.
    #[error("unknown policy {0}")]
    UnknownPolicy(String),
}
